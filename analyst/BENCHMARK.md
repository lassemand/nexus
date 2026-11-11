# analyst kernel runtime: benchmark

Measured with `analyst-bench` on the machine the analyst service will run on
(see below), 2026-11-11. This is the kernel runtime of NEX-251: a `wasmtime`
engine with Cranelift `OptLevel::Speed`, epoch interruption and SIMD, no WASI
and no fuel, one instance per kernel, warmed at load.

## Machine

* The conductor pod on the Mac mini's OrbStack VM: Linux 7.2.9 aarch64, Apple
  CPU, 10 vCPUs, 10 GB.
* **Not an idle machine.** The VM also runs Kafka, Postgres, Prometheus and
  the other services; the load average was about 20 on 10 CPUs while these
  runs were taken, so p90 and above carry noise. Four runs are listed below to
  show it; run 1 is the noisiest.
* It is the Linux VM of the production host, not macOS on the Mac mini. A rerun
  on an otherwise idle host should be faster; that was not tried.

## What was measured

* **Kernels:** three benchmark kernels for one example company
  (`verdict/kernels/bench-anchored`, `bench-memmem`, `bench-aho`), each
  entirely its own lookup: known paragraph prefixes with a `memmem` fallback,
  one `memmem::Finder` per label in a 6 KB window, one Aho-Corasick pass. They
  read the same three metrics and share the reading code and the decision rule.
  A fourth, `verdict-sample`, is the skeleton that looks nothing up.
* **Example company:** its last four quarterly reports in Norwegian and English
  (8 releases, 3–4.5 KB each, `verdict/kernels/bench-fixtures/`).
* **Corpus:** 2,399 MFN interim-report releases from
  `verdict/fetch-corpus.sh` (24.6 MB, up to 220 KB), in several languages. The
  aho kernel runs over all of it as the worst case for scanning a whole report;
  on other companies' reports it abstains and only the time is measured.
* **Per release:** one call of the kernel on one text, p50/p90/p99/max over all
  releases times 15 iterations. WASM includes copying the text in, running the
  kernel, and reading the JSON out; native includes `run_into` (the same JSON).
* Commands:

```bash
cargo build --release --target wasm32-unknown-unknown \
  -p verdict-bench-anchored -p verdict-bench-memmem -p verdict-bench-aho -p verdict-sample
cargo build --release -p analyst --features bench
verdict/fetch-corpus.sh /tmp/corpus 2400
target/release/analyst-bench --corpus /tmp/corpus --example verdict/kernels/bench-fixtures/lsg --iters 15
```

## Result (run 2 of 4)

* example releases: 8 (29957 bytes), corpus releases: 2399 (24572121 bytes)
* iterations: 15, deadline: 50ms, wasmtime epoch tick 1 ms

### Setup costs (once per kernel)

| kernel | wasm bytes | validate (ms) | compile (ms) | instantiate (ms) | first calls = warm (ms) |
| -- | -- | -- | -- | -- | -- |
| anchored | 95213 | 0.703 | 54.210 | 0.050 | 0.032 |
| memmem | 87660 | 0.381 | 43.408 | 0.056 | 0.022 |
| aho | 252150 | 0.862 | 131.364 | 0.049 | 0.136 |
| skeleton (no lookup) | 27344 | 0.123 | 12.316 | 0.025 | 0.020 |

* byte-identical WASM vs native, anchored: yes, every release
* byte-identical WASM vs native, memmem: yes, every release
* byte-identical WASM vs native, aho: yes, every release

### Time per release (microseconds)

| kernel | runtime | releases | p50 | p90 | p99 | max | MB/s |
| -- | -- | -- | -- | -- | -- | -- | -- |
| anchored, example company | native | 8 | 1.46 | 1.75 | 3.33 | 5.54 | 2412 |
| anchored, example company | wasm | 8 | 3.67 | 4.42 | 5.88 | 16.34 | 998 |
| memmem, example company | native | 8 | 1.50 | 1.71 | 2.79 | 4.58 | 2420 |
| memmem, example company | wasm | 8 | 3.79 | 4.71 | 7.75 | 14.96 | 950 |
| aho, example company | native | 8 | 3.08 | 4.13 | 8.25 | 10.08 | 1128 |
| aho, example company | wasm | 8 | 10.96 | 15.46 | 19.92 | 20.25 | 317 |
| aho, whole corpus | native | 2399 | 5.83 | 19.84 | 68.55 | 272.46 | 1063 |
| aho, whole corpus | wasm | 2399 | 18.54 | 58.59 | 183.23 | 498.78 | 354 |
| skeleton (empty kernel), example company | wasm | 8 | 0.54 | 0.71 | 1.38 | 4.42 | 6212 |

### Fixed per-call cost

The skeleton kernel looks nothing up, so its p50 of 0.54 µs is the cost of one call through the runtime (copy the text in, run, read the JSON out). The anchored kernel's p50 is 3.67 µs, so the fixed part is 15 % of it.

### Pass criteria

* anchored, wasm p50 3.67 µs <= 5 µs: PASS
* aho over the corpus, wasm p50 18.54 µs <= 30 µs: PASS
* aho over the corpus, wasm p99 183.23 µs <= 400 µs: PASS
* every kernel byte-identical to native: PASS

## All four runs (WASM, microseconds, p50 / p99)

| run | empty kernel p50 | anchored | memmem window | aho (example) | aho (whole corpus) |
| -- | -- | -- | -- | -- | -- |
| 1 | 1.67 | 4.71 / 19.34 | 4.50 / 29.88 | 12.21 / 56.17 | 22.25 / 236.37 |
| 2 | 0.54 | 3.67 / 5.88 | 3.79 / 7.75 | 10.96 / 19.92 | 18.54 / 183.23 |
| 3 | 0.54 | 3.88 / 53.63 | 8.71 / 60.55 | 13.08 / 97.01 | 18.79 / 183.53 |
| 4 | 0.54 | 3.79 / 6.17 | 3.96 / 9.42 | 10.21 / 19.38 | 21.34 / 259.66 |

## Reading the results

* **Pass criteria:** met in every run: anchored WASM p50 at most 5 µs (3.7–4.7),
  aho over the corpus p50 at most 30 µs (18.5–22.3) and p99 at most 400 µs
  (183–260), and every kernel byte-identical to native on every release (the
  example releases and all 2,399 corpus releases).
* **Runtime overhead:** the empty kernel takes 0.5 µs per call in the three
  quieter runs (1.7 µs in run 1), far under the 50 µs target. A warmed
  instance's first real call is a normal call: the two warm-up calls take
  0.02–0.17 ms in total, under the 1 ms target.
* **Fixed per-call cost:** 0.5 µs of the anchored kernel's 3.7 µs, about 15 %.
  It does not dominate, so no cheaper output path is proposed. The rest is the
  kernel itself: finding the paragraphs, reading up to six figures and
  serialising the metrics to JSON with `Decimal` strings.
* **Compile:** 12 ms for the skeleton, 43–54 ms for the 90 KB anchored and
  memmem kernels, 130 ms for the 250 KB aho kernel (run 2), done once at load
  from the `.wasm` bytes; nothing precompiled is stored.
* **WASM against native:** about 2.5× for the small kernels and 3× for aho over
  the corpus, more than the 1.5× of the earlier prototype run. The kernels are
  built without `simd128`, and the host was loaded; whether either explains the
  gap was not investigated.
* **Anchored is not under 1 µs.** The guide expected an anchored lookup well
  under 1 µs; measured, the whole call is 3.7 µs. The reading and the JSON
  output are as much work as the search. It is still about 3× faster than one
  Aho-Corasick pass on the same reports (10–13 µs).
* **Slow tail:** the max over the corpus is 500–3100 µs across the four runs,
  against a p99 of 183–260 µs. The cause (a large release, or a descheduled
  thread on the busy host) was not investigated.
