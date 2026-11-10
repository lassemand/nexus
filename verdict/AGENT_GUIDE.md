# Kernel agent guide

You are writing a **kernel**: a small Rust crate that reads one company's
quarterly or annual report release and returns a verdict (`buy`, `no_buy` or
`abstain`) with the metrics it found. It is written about 12 hours before the
report, compiled to WASM, run once by the `analyst` service inside `wasmtime`
when the release arrives, then kept for audit. The decision feeds a
**simulated** trade.

You edit `verdict/template` (crate `verdict-kernel`). The worked example is
`verdict/kernels/sample` (crate `verdict-sample`). The SDK (`verdict/sdk`) holds
only the ABI, the types and a test and timing harness.

## The principle

**You decide how this report is read.** The SDK has no search and no number
parsing; that is your code. You have read this company's previous reports, so
you know where the figures are. Speed is key: choose the **cheapest lookup that
the previous reports prove is stable**, verify it cheaply, and fall back to a
broader one only when the verification fails. A simple lookup beats text search
whenever one is possible.

## What you may use

* `verdict-sdk` (`Report`, `Lang`, `Metric`, `Decision`, `Verdict`,
  `export_kernel!`, `Decimal`; the harness in tests).
* Only if your lookup needs them: `memchr`, `aho-corasick`, `regex` (all
  vendored; no approval needed).
* No other dependencies, no build scripts, no proc-macro crates.
* Keep the template's `crate-type = ["cdylib", "rlib"]`, its release profile and
  the `export_kernel!(crate::analyze)` line.

Build and test:

```bash
cd verdict/template
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release --target wasm32-unknown-unknown
```

## The contract

```rust
pub fn analyze(report: &Report) -> Verdict
```

* `report.text` is the release: the title, a blank line, then MFN's
  `content.text`. `report.lang` is its `Lang` (`En`, `Sv`, `Da`, `Fi`, `No`,
  `Other`).
* Return a `Verdict { decision, reason, metrics }`. `reason` is one short
  sentence. `metrics` lists every metric you found, as a `Metric { key, current,
  prior, expected, unit }`, with values as exact decimal strings
  (`value.to_string()`).
* `export_kernel!` exports the WASM ABI (`verdict_input`, `verdict_run`,
  `memory`; no imports). Don't write your own exports.

## The lookup ladder

Use the first rung that works on this company's previous reports, **in every
language it publishes in**.

1. **Structural position.** The figure is always in the title, or on the first
   bullet line after the heading. Find the line with `memchr(b'\n')` (a few
   calls), check its expected prefix with `starts_with`, then read the number
   directly at a known offset. This touches a few hundred bytes.
2. **A single anchor in a known window.** One `memchr::memmem::Finder`, built
   once in a `static LazyLock`, over just the window where previous reports had
   it (e.g. the first 2 KB), with the exact casing the company uses.
3. **Several labels.** One Aho-Corasick pass, built once, with early exit.
4. **Regex.** Only if nothing above works, compiled once.

`verdict/kernels/sample` implements all of them as variants of one kernel, so
you can compare: (a) anchored with a fallback to (c), (b) `memmem` per label in
the first 2 KB, (c) Aho-Corasick, (d) regex, (e) `to_lowercase` + `find`.

### Verify, and have a fallback

Every lookup checks that it found what it expected: the prefix matched, the
number parsed, the unit is right (a margin is a percentage, an amount is not).

* If a rung-1 or rung-2 check fails (the company changed its layout), fall back
  to a broader lookup that you also wrote.
* If that fails too, `Abstain` with the reason (`"net sales not found"`).
* **Never return a number you did not verify.**

Test the fallback with a fixture whose first line or a label prefix was changed.

### Measure

Use `harness::time_per_release` on the previous reports, for your chosen lookup
and for your fallback, and record p50/p99 in `kernel.json`. State there why a
faster rung did not work. Measured on 1,555 real MFN interim-report releases
(en/sv/fi/da; median 6.9 KB, p90 21 KB, max 220 KB; Apple M4 Pro):

| Search approach | native p50 | WASM p50 | WASM p99 |
| -- | -- | -- | -- |
| one Aho-Corasick pass over all labels, built once | 10 µs | 15 µs | 190 µs |
| one regex per metric, compiled once | 12 µs | 38 µs | 382 µs |
| `memmem::Finder` per phrase, built once | 19 µs | 48 µs | 592 µs |
| `to_lowercase()` + `str::find` | 53 µs | 127 µs | 1,587 µs |
| regex compiled on every call | 676 µs | 2,434 µs | 2,825 µs |

An anchored lookup is expected to be well under 1 µs, because it touches a few
hundred bytes instead of the whole release. WASM costs about 1.5× native. A
fresh instance's first call (building the statics) takes about 0.4 ms, against
15–20 µs warm. The kernel is under 0.1 % of release-to-decision time (MFN
delivery takes 0.5–2 s, a Saxo quote 50–200 ms): don't make it slower without a
reason, and never trade correctness for speed.

## Reading numbers exactly

These are rules for **your own code**, written for this company's format. The
sample's `number.rs` is one worked implementation; copy what you need.

* `Decimal` only, never `f64`. Build values exactly (a mantissa and a scale),
  use `checked_div` and check a divisor for zero. Compare signs explicitly:
  going from −11.7 to −6.1 is an improvement (`-6.1 > -11.7`).
* Know the company's separators: `67.3`, `67,3`, `1 234,5` (with no-break,
  narrow no-break or thin spaces), `1.234,5`. `12,345` is 12.345 in Swedish and
  12345 in English.
* Minus signs: `-`, `−` (U+2212) and `–` (U+2013). A dash between two figures
  (`2025–2026`) is a range, not a minus.
* **Parentheses mean the prior period, not a negative number:** `67.3 (72.7)` is
  this year (last year).
* Skip footnote markers (`¹`, `^{1}`, `[1]`) and period labels (`Q3`, `H1`) and
  years, if the layout has them.
* A percentage before the amount (`increased by 12% to SEK 812.4 million`) is
  not the amount.
* Test each of these on the fixtures.

## Speed rules, with the measured reasons

* **Build anything compiled** (`Finder`, automaton, regex) **once**, in a
  `static LazyLock`. Building per call is 65–160× slower than the search, and
  the host warms the kernel at load, so don't defer other work to the first real
  call.
* **Never copy or lowercase the text.** It is 5× slower, and offsets shift when
  a character changes length, so figures are read from the wrong place.
* `memmem` is **case-sensitive**: use the company's exact casing, or a
  case-insensitive automaton.
* **Allocate only the `Verdict`.** Read offsets and `Decimal`s; build strings
  only for the returned `Metric`s and `reason`.
* **Never panic.** No `unwrap`, no `expect`, no out-of-bounds indexing
  (`slice[i]`, `&text[a..b]`): use `get`. The kernel is built with
  `panic = "abort"`; a panic is a trap, and the host gets no verdict at all.

## Lessons from real reports

Keep these in mind whenever your lookup is broader than a fixed line:

* A label after a bullet `•`, a dash `–` or a no-break space must still be
  found. Treat anything that is not alphabetic and not an ASCII digit as a word
  boundary; `å`/`ö` are letters.
* `revenue` matches inside `revenues`: use an open stem or a boundary check.
* `EBIT margin` is not EBIT; the longer label must win at the same position.
* "Periodens resultat per aktie" is EPS, not net income (the same overlap:
  `periodens resultat` is a prefix of it).
* Table column order varies by company (one shows the prior year first, e.g.
  `3.73 │4.24`): check the header in previous reports and hard-code the column.
* MFN wraps lines at about 80 columns. A label or a `(prior)` can be split by a
  line break; allow one break, never a blank line.
* Non-ASCII letters are not case-folded by ASCII-only matching: list both `Ø`/`ø`
  where a phrase can start a sentence.
* The first occurrence is not always the figure: a title or intro may mention
  "revenue growth" without one. Anchor on a phrase that only appears next to the
  figure (`net sales amounted to`).
* Use the company's own wording from its previous reports. Banks report net
  interest income, property companies rental income, biotech often no sales.

## Testing

* Save the company's previous releases in `tests/fixtures/` as
  `<lang>__<slug>__<id>.txt` (title, blank line, `content.text`) and add a case
  per fixture in `tests/kernel.rs`. Use `harness::check_fixtures` (or
  `assert_fixtures`) on **the last two reports in each language**; it asserts
  every metric's current and prior value and the decision, and prints an exact
  per-value diff.
* `harness::assert_abstains` on **another company's report**: it must give
  `abstain`, not nonsense.
* **The WASM output must equal the native output byte for byte.** The host runs
  the WASM build; native tests only prove it if both agree.
* Run `cargo clippy --all-targets -- -D warnings` and build the WASM release
  before you finish, and fill in `kernel.json`.

## Corpus

`verdict/fetch-corpus.sh [OUT_DIR] [MAX]` downloads MFN interim reports into the
gitignored `verdict/corpus/` as `<lang>__<slug>__<id>.txt`. It is for the
benchmark and for finding other companies' wording; never commit it.
