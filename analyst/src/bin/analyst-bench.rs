//! Benchmarks the bench kernels natively and through `KernelRuntime`, and
//! checks that the WASM and native JSON are byte-identical for every release.
//!
//! ```text
//! analyst-bench --corpus <dir> --example <dir> [--wasm <dir>] [--iters 15]
//! ```
//!
//! `--corpus` holds MFN releases named `<lang>__<slug>__<id>.txt`
//! (`verdict/fetch-corpus.sh`), `--example` the example company's releases in
//! the same naming, `--wasm` the built kernels (default
//! `target/wasm32-unknown-unknown/release`). Build the kernels first:
//!
//! ```text
//! cargo build --release --target wasm32-unknown-unknown \
//!   -p verdict-bench-anchored -p verdict-bench-memmem -p verdict-bench-aho -p verdict-sample
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use analyst::{KernelRuntime, LoadTimings, RuntimeConfig};
use anyhow::{bail, Context, Result};
use verdict_sdk::{run_into, Analyze, Lang};

struct Release {
    name: String,
    lang: Lang,
    text: String,
}

struct Args {
    corpus: PathBuf,
    example: PathBuf,
    wasm: PathBuf,
    iters: usize,
}

fn parse_args() -> Result<Args> {
    let mut corpus = None;
    let mut example = None;
    let mut wasm = PathBuf::from("target/wasm32-unknown-unknown/release");
    let mut iters = 15usize;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || args.next().with_context(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--corpus" => corpus = Some(PathBuf::from(value()?)),
            "--example" => example = Some(PathBuf::from(value()?)),
            "--wasm" => wasm = PathBuf::from(value()?),
            "--iters" => iters = value()?.parse().context("--iters must be a number")?,
            other => bail!("unknown argument {other}"),
        }
    }
    Ok(Args {
        corpus: corpus.context("--corpus <dir> is required")?,
        example: example.context("--example <dir> is required")?,
        wasm,
        iters: iters.max(1),
    })
}

fn load_releases(dir: &Path) -> Result<Vec<Release>> {
    let mut releases = Vec::new();
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .collect();
    names.sort();
    for path in names {
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_owned();
        let lang = Lang::from_tag(name.split("__").next().unwrap_or(""));
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        if let Ok(text) = String::from_utf8(bytes) {
            releases.push(Release { name, lang, text });
        }
    }
    Ok(releases)
}

struct Stats {
    p50: Duration,
    p90: Duration,
    p99: Duration,
    max: Duration,
    mb_per_s: f64,
}

fn stats(samples: &mut [Duration], bytes: usize) -> Stats {
    samples.sort();
    let at = |percent: usize| {
        let index = (samples.len() * percent / 100).min(samples.len().saturating_sub(1));
        samples.get(index).copied().unwrap_or(Duration::ZERO)
    };
    let total: Duration = samples.iter().sum();
    let mb_per_s = if total.is_zero() {
        0.0
    } else {
        bytes as f64 / 1_000_000.0 / total.as_secs_f64()
    };
    Stats {
        p50: at(50),
        p90: at(90),
        p99: at(99),
        max: samples.last().copied().unwrap_or(Duration::ZERO),
        mb_per_s,
    }
}

fn micros(duration: Duration) -> String {
    format!("{:.2}", duration.as_secs_f64() * 1e6)
}

fn row(label: &str, runtime: &str, stats: &Stats, releases: usize) -> String {
    format!(
        "| {label} | {runtime} | {releases} | {} | {} | {} | {} | {:.0} |",
        micros(stats.p50),
        micros(stats.p90),
        micros(stats.p99),
        micros(stats.max),
        stats.mb_per_s
    )
}

fn native_samples(analyze: Analyze, releases: &[Release], iters: usize) -> Vec<Duration> {
    let mut out = Vec::with_capacity(4096);
    let mut samples = Vec::with_capacity(releases.len() * iters);
    for _ in 0..iters {
        for release in releases {
            let started = Instant::now();
            run_into(
                analyze,
                release.text.as_bytes(),
                release.lang.code(),
                &mut out,
            );
            samples.push(started.elapsed());
        }
    }
    samples
}

fn wasm_samples(
    kernel: &analyst::LoadedKernel,
    releases: &[Release],
    iters: usize,
) -> Result<Vec<Duration>> {
    let mut samples = Vec::with_capacity(releases.len() * iters);
    for _ in 0..iters {
        for release in releases {
            let (_, elapsed) = kernel
                .run_with(release.text.as_bytes(), release.lang, |_| Ok(()))
                .map_err(|error| anyhow::anyhow!("{}: {error}", release.name))?;
            samples.push(elapsed);
        }
    }
    Ok(samples)
}

fn identical(
    analyze: Analyze,
    kernel: &analyst::LoadedKernel,
    releases: &[Release],
) -> Result<usize> {
    let mut mismatches = 0;
    let mut native = Vec::new();
    for release in releases {
        run_into(
            analyze,
            release.text.as_bytes(),
            release.lang.code(),
            &mut native,
        );
        let (wasm, _) = kernel
            .run_with(release.text.as_bytes(), release.lang, |bytes| {
                Ok(bytes.to_vec())
            })
            .map_err(|error| anyhow::anyhow!("{}: {error}", release.name))?;
        if wasm != native {
            mismatches += 1;
            eprintln!("MISMATCH {}: wasm and native JSON differ", release.name);
        }
    }
    Ok(mismatches)
}

fn bytes_of(releases: &[Release]) -> usize {
    releases.iter().map(|release| release.text.len()).sum()
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<bool> {
    let args = parse_args()?;
    let corpus = load_releases(&args.corpus)?;
    let example = load_releases(&args.example)?;
    if corpus.is_empty() || example.is_empty() {
        bail!("the corpus and the example directory must each hold .txt releases");
    }
    let runtime = KernelRuntime::new(RuntimeConfig::default())?;
    let kernels: [(&str, &str, Analyze); 3] = [
        (
            "anchored",
            "verdict_bench_anchored.wasm",
            verdict_bench_anchored::analyze,
        ),
        (
            "memmem",
            "verdict_bench_memmem.wasm",
            verdict_bench_memmem::analyze,
        ),
        ("aho", "verdict_bench_aho.wasm", verdict_bench_aho::analyze),
    ];
    let mut loaded = Vec::new();
    for (index, (name, file, analyze)) in kernels.iter().enumerate() {
        let path = args.wasm.join(file);
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let kernel = runtime
            .load(index as u64 + 1, &bytes)
            .with_context(|| format!("loading {name}"))?;
        loaded.push((*name, *analyze, kernel));
    }
    let sample_path = args.wasm.join("verdict_sample.wasm");
    let sample = runtime.load(
        99,
        &std::fs::read(&sample_path)
            .with_context(|| format!("reading {}", sample_path.display()))?,
    )?;

    println!("# analyst-bench\n");
    println!(
        "* example releases: {} ({} bytes), corpus releases: {} ({} bytes)",
        example.len(),
        bytes_of(&example),
        corpus.len(),
        bytes_of(&corpus)
    );
    println!(
        "* iterations: {}, deadline: {:?}, wasmtime epoch tick 1 ms\n",
        args.iters,
        RuntimeConfig::default().deadline
    );

    println!("## Setup costs (once per kernel)\n");
    println!("| kernel | wasm bytes | validate (ms) | compile (ms) | instantiate (ms) | first calls = warm (ms) |");
    println!("| -- | -- | -- | -- | -- | -- |");
    let ms = |d: Duration| format!("{:.3}", d.as_secs_f64() * 1e3);
    for (name, _, kernel) in &loaded {
        let LoadTimings {
            validate,
            compile,
            instantiate,
            warm,
        } = kernel.timings();
        println!(
            "| {name} | {} | {} | {} | {} | {} |",
            kernel.size(),
            ms(validate),
            ms(compile),
            ms(instantiate),
            ms(warm)
        );
    }
    let t = sample.timings();
    println!(
        "| skeleton (no lookup) | {} | {} | {} | {} | {} |\n",
        sample.size(),
        ms(t.validate),
        ms(t.compile),
        ms(t.instantiate),
        ms(t.warm)
    );

    let mut failed = false;
    let mut mismatches = 0;
    for (name, analyze, kernel) in &loaded {
        let bad = identical(*analyze, kernel, &example)? + identical(*analyze, kernel, &corpus)?;
        mismatches += bad;
        println!(
            "* byte-identical WASM vs native, {name}: {}",
            if bad == 0 {
                "yes, every release".to_owned()
            } else {
                format!("NO, {bad} releases differ")
            }
        );
    }
    if mismatches > 0 {
        failed = true;
    }

    println!("\n## Time per release (microseconds)\n");
    println!("| kernel | runtime | releases | p50 | p90 | p99 | max | MB/s |");
    println!("| -- | -- | -- | -- | -- | -- | -- | -- |");
    let mut anchored_wasm = Duration::ZERO;
    let mut aho_corpus = (Duration::ZERO, Duration::ZERO);
    for (name, analyze, kernel) in &loaded {
        let mut native = native_samples(*analyze, &example, args.iters);
        let native_stats = stats(&mut native, bytes_of(&example) * args.iters);
        println!(
            "{}",
            row(
                &format!("{name}, example company"),
                "native",
                &native_stats,
                example.len()
            )
        );
        let mut wasm = wasm_samples(kernel, &example, args.iters)?;
        let wasm_stats = stats(&mut wasm, bytes_of(&example) * args.iters);
        println!(
            "{}",
            row(
                &format!("{name}, example company"),
                "wasm",
                &wasm_stats,
                example.len()
            )
        );
        if *name == "anchored" {
            anchored_wasm = wasm_stats.p50;
        }
        if *name == "aho" {
            let mut native = native_samples(*analyze, &corpus, args.iters);
            let native_stats = stats(&mut native, bytes_of(&corpus) * args.iters);
            println!(
                "{}",
                row("aho, whole corpus", "native", &native_stats, corpus.len())
            );
            let mut wasm = wasm_samples(kernel, &corpus, args.iters)?;
            let wasm_stats = stats(&mut wasm, bytes_of(&corpus) * args.iters);
            println!(
                "{}",
                row("aho, whole corpus", "wasm", &wasm_stats, corpus.len())
            );
            aho_corpus = (wasm_stats.p50, wasm_stats.p99);
        }
    }
    let mut fixed = wasm_samples(&sample, &example, args.iters)?;
    let fixed_stats = stats(&mut fixed, bytes_of(&example) * args.iters);
    println!(
        "{}",
        row(
            "skeleton (empty kernel), example company",
            "wasm",
            &fixed_stats,
            example.len()
        )
    );

    println!("\n## Fixed per-call cost\n");
    let share = if anchored_wasm.is_zero() {
        0.0
    } else {
        fixed_stats.p50.as_secs_f64() / anchored_wasm.as_secs_f64() * 100.0
    };
    println!("The skeleton kernel looks nothing up, so its p50 of {} µs is the cost of one call through the runtime (copy the text in, run, read the JSON out). The anchored kernel's p50 is {} µs, so the fixed part is {:.0} % of it.\n", micros(fixed_stats.p50), micros(anchored_wasm), share);

    println!("## Pass criteria\n");
    let check = |label: &str, ok: bool| println!("* {label}: {}", if ok { "PASS" } else { "FAIL" });
    check(
        &format!("anchored, wasm p50 {} µs <= 5 µs", micros(anchored_wasm)),
        anchored_wasm <= Duration::from_micros(5),
    );
    check(
        &format!(
            "aho over the corpus, wasm p50 {} µs <= 30 µs",
            micros(aho_corpus.0)
        ),
        aho_corpus.0 <= Duration::from_micros(30),
    );
    check(
        &format!(
            "aho over the corpus, wasm p99 {} µs <= 400 µs",
            micros(aho_corpus.1)
        ),
        aho_corpus.1 <= Duration::from_micros(400),
    );
    check("every kernel byte-identical to native", mismatches == 0);
    Ok(!failed)
}
