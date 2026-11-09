# Kernel agent guide

You are writing a **kernel**: a small Rust crate that reads one company's
quarterly or annual report release and returns a verdict (`buy`, `no_buy` or
`abstain`) with the metrics it found. It is written about 12 hours before the
report, compiled to WASM, run once by the `analyst` service inside `wasmtime`
when the release arrives, then kept for audit. The decision feeds a
**simulated** trade.

You edit `verdict/template` (crate `verdict-kernel`). The worked example is
`verdict/kernels/sample` (crate `verdict-sample`). Both use only
`verdict-sdk` (`verdict/sdk`).

## What you may use

* Only `verdict-sdk`. It re-exports `aho_corasick`, `memchr`, `regex` and
  `Decimal` (`rust_decimal`).
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
  (`num.value.to_string()`).
* `export_kernel!` exports the WASM ABI (`verdict_input`, `verdict_run`,
  `memory`; no imports). Don't write your own exports.

## Why speed matters, and what was measured

Measured on 1,555 real MFN interim-report releases (en/sv/fi/da; median 6.9 KB,
p90 21 KB, max 220 KB; Apple M4 Pro):

| Search approach | native p50 | WASM p50 | WASM p99 |
| -- | -- | -- | -- |
| one Aho-Corasick pass over all labels (`Labels`), built once | 10 µs | 15 µs | 190 µs |
| one regex per metric, compiled once | 12 µs | 38 µs | 382 µs |
| `memmem::Finder` per phrase, built once | 19 µs | 48 µs | 592 µs |
| `to_lowercase()` + `str::find` | 53 µs | 127 µs | 1,587 µs |
| regex compiled on every call | 676 µs | 2,434 µs | 2,825 µs |

* WASM costs about 1.5× native; `simd128` doesn't change the `Labels` time.
* A fresh instance's first call (building the statics) takes about 0.4 ms,
  against 15–20 µs warm.
* The kernel is under 0.1 % of release-to-decision time (MFN delivery takes
  0.5–2 s, a Saxo quote 50–200 ms). Don't make it slower without a reason,
  and never trade correctness for speed.

## Rules

1. **One pass.** Find every label in one pass with `Labels`. Don't loop `find`
   per phrase, and don't use regex to *find* labels. *Why:* one Aho-Corasick
   pass is 15 µs in WASM; a regex per metric is 2.5× and a `Finder` per phrase
   3× slower, and both grow with every phrase you add.
2. **Build once.** Build `Labels` (and any `Regex`) once, in a
   `static LazyLock`. *Why:* building per call is 65–160× slower than the
   search. The host warms the kernel at load (first call about 0.4 ms with
   `Labels`, 2.8 ms with regexes; warm about 18 µs), so don't defer other work to
   the first real call.
3. **Don't copy or lowercase the text.** *Why:* `to_lowercase()` + `find` is 5×
   slower, and offsets shift when a character changes length, so figures are
   then read from the wrong place. `Labels` is already ASCII case-insensitive.
4. **Read figures with the SDK.** Use `next_number` / `current_and_prior` from
   `hit.end` with a window of about 200 bytes, and `unit_after` for the unit.
   Parentheses mean the prior period: `67.3 (72.7)` is this year (last year),
   **not** a negative number. *Why:* the SDK already handles thousands
   separators (including no-break and thin spaces), minus signs (`-`, `−`,
   `–`), ranges (`2025–2026`), years, period names (`Q3`, `H1`) and footnote
   markers (`^{1}`, `[1]`), which each broke hand-written parsing on real
   releases.
5. **Pass the language's number style.** Use `report.lang.number_style()`.
   *Why:* `12,345` is 12.345 in Swedish and 12345 in English; `1.234,5` is
   Danish.
6. **Decimals only.** Use `Decimal` for every value and comparison, never
   `f64`. Use `checked_div` and check a divisor for zero. Compare signs
   explicitly: going from −11.7 to −6.1 is an improvement (`-6.1 > -11.7`).
   *Why:* floats round (`0.1 + 0.1 + 0.1 != 0.3`), and the verdict must
   reproduce exactly.
7. **Never panic.** No `unwrap`, no `expect`, no out-of-bounds indexing
   (`slice[i]`, `&text[a..b]`): use `get`. A missing metric means `Abstain`
   with the reason (`"net sales not found"`). *Why:* the kernel is built with
   `panic = "abort"`; a panic is a trap, and the host gets no verdict at all.
8. **Allocate only the `Verdict`.** Read offsets and `Decimal`s, and build
   strings only for the returned `Metric`s and `reason`. *Why:* everything
   else is work on the release's hot path; copying the text was part of the 5×
   cost of the `to_lowercase()` row above, and the warm `Labels` search itself
   allocates only the hit vector.

## Phrase lists

`Labels::new(&PHRASES)` takes one list of phrases per metric, in the order of
your `KEYS`. `first_hits(text)` returns the first valid hit of each metric.

* **Use open stems.** `revenue*` also matches `revenues`, and
  `nettoomsättning*` also matches `nettoomsättningen`. Use an open stem rather
  than listing inflections. A phrase without `*` must end at a word boundary.
* **Non-ASCII letters match exactly.** Only ASCII is case-insensitive, so list
  both `Ø`/`ø`, `Å`/`å`, `Ä`/`ä` forms where a phrase can start a sentence
  (`Årets resultat`, `årets resultat`).
* **The longest phrase at the same byte wins, across metrics.** `ebit margin`
  correctly hides `ebit` in "EBIT margin 9.8%". But `periodens resultat` (net
  income) also hides `resultat per aktie` (EPS) in "Periodens resultat per
  aktie", because the EPS phrase starts later. **Add the long form to the right
  metric**: the sample lists `periodens resultat per aktie` under `eps`.
* **Cover every language the company publishes in.** Check its previous
  releases; many publish Swedish and English, Finnish companies often Finnish,
  Swedish and English.
* **A label split across a line break is not matched.** MFN wraps text at about
  80 columns; if a previous report wrapped a label, add the phrase that appears
  before the break, or a shorter phrase.
* **Use the company's own wording** from its previous reports. Banks report net
  interest income, property companies rental income, biotech often no sales at
  all. The first hit is the *first* occurrence: if the title or an intro
  sentence mentions "revenue growth" without a figure, pick a phrase that only
  appears next to the figure (`net sales amounted to`).
* Boundaries are decided on the character: anything that is not alphabetic and
  not an ASCII digit is a boundary, so bullets `•`, dashes `–`, no-break spaces
  and superscripts `¹` are boundaries (`Earnings per share¹` ends at `share`),
  and `å`/`ö` are letters.

## Tables

The summary bullets are the primary source. Table column order varies by
company (one shows the prior year first, e.g. `3.73 │4.24`), so use a table
only after checking its header in the company's previous reports, with the
column index hard-coded. Read a table row with repeated `next_number` calls from
the row label's `hit.end`.

## Worked example

From `verdict/kernels/sample/src/lib.rs` (shortened):

```rust
pub const KEYS: [&str; 5] = ["ebit_margin", "net_sales", "ebit", "net_income", "eps"];

pub const PHRASES: [&[&str]; 5] = [
    &["ebit margin", "operating margin", "rörelsemarginal*", /* ... */],
    &["net sales", "revenue*", "nettoomsättning*", /* ... */],
    &["ebit", "operating profit", "rörelseresultat*", /* ... */],
    &["profit for the period", "periodens resultat", /* ... */],
    &["earnings per share", "resultat per aktie", "periodens resultat per aktie", /* ... */],
];

static LABELS: LazyLock<Labels> = LazyLock::new(|| Labels::new(&PHRASES));

pub fn analyze(report: &Report) -> Verdict {
    let style = report.lang.number_style();
    for (key, hit) in KEYS.iter().zip(LABELS.first_hits(report.text)) {
        let Some(hit) = hit else { continue };
        let Some((current, prior)) = current_and_prior(report.text, hit.end, 200, style) else {
            continue;
        };
        let unit = unit_after(report.text, current.end);
        /* ... build a Metric ... */
    }
    /* ... decide with Decimal comparisons, Abstain when a metric is missing ... */
}

export_kernel!(crate::analyze);
```

The sample also skips a figure followed by `%` when it reads an amount, so
"Net sales increased by 12% to SEK 812.4 million (725.1)" gives 812.4, not 12.
Its decision rule is a placeholder; yours comes from the task.

## Testing

* Save the company's previous releases in `tests/fixtures/` as
  `<lang>__<slug>__<id>.txt` (title, blank line, `content.text`) and add a case
  per fixture in `tests/kernel.rs`. **Assert every metric's current and prior
  value on the company's last two reports in each language.**
* **A release from another company must give `abstain`, not nonsense.** Keep the
  template's foreign-release test and make it pass.
* **The WASM output must equal the native output byte for byte.** The host runs
  the WASM build; the native tests only prove it if both agree.
* Run `cargo clippy --all-targets -- -D warnings` and build the WASM release
  before you finish.

## Corpus

`verdict/fetch-corpus.sh [OUT_DIR] [MAX]` downloads MFN interim reports into the
gitignored `verdict/corpus/` as `<lang>__<slug>__<id>.txt`. It is for the
benchmark and for finding other companies' wording; never commit it.
