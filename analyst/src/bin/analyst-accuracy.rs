//! Extraction accuracy of dry-run kernels (`docs/validation/analyst-accuracy.md`).
//!
//! `inputs` writes the dry-run `PREP_INPUT` of every fixture, to start one
//! preparation Job per fixture with `PREP_DRY_RUN=true`. `score` runs each
//! fixture's kernel (`<fixture>.wasm`) on the fixture's release through the
//! analyst's own runtime and scores its figures against the hand-checked ones.
//! Neither touches a database, Kafka or a quote: nothing here can create a
//! trade.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use analyst::accuracy::{load_all, render_markdown, score, KernelResult, Summary};
use analyst::{KernelRuntime, RunOutcome, RuntimeConfig};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::{error, info, warn};

/// Extraction accuracy of dry-run kernels.
#[derive(Debug, Parser)]
#[command(name = "analyst-accuracy")]
struct Cli {
    /// The fixture directory.
    #[arg(long, default_value = "analyst/tests/accuracy")]
    fixtures: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Writes `<out>/<fixture>.json`, the dry-run PREP_INPUT of each fixture.
    Inputs {
        /// Where to write the inputs.
        #[arg(long)]
        out: PathBuf,
    },
    /// Scores `<kernels>/<fixture>.wasm` on each fixture; exits 1 unless the run passes.
    Score {
        /// The directory of the dry-run kernels.
        #[arg(long)]
        kernels: PathBuf,
        /// Also writes the markdown result here.
        #[arg(long)]
        markdown: Option<PathBuf>,
        /// Also writes every scored value as JSON here.
        #[arg(long)]
        json: Option<PathBuf>,
    },
}

fn write_inputs(fixtures: &Path, out: &Path) -> Result<()> {
    let fixtures = load_all(fixtures)?;
    std::fs::create_dir_all(out).with_context(|| format!("cannot create {}", out.display()))?;
    for fixture in &fixtures {
        let path = out.join(format!("{}.json", fixture.name));
        let json = serde_json::to_string_pretty(&fixture.dry_run_input())?;
        std::fs::write(&path, json).with_context(|| format!("cannot write {}", path.display()))?;
    }
    info!(fixtures = fixtures.len(), out = %out.display(), "dry-run inputs written");
    Ok(())
}

fn run_kernel(
    runtime: &KernelRuntime,
    id: u64,
    wasm: &Path,
    text: &str,
    lang: verdict_sdk::Lang,
) -> KernelResult {
    let bytes = match std::fs::read(wasm) {
        Ok(bytes) => bytes,
        Err(error) => return KernelResult::Failed(format!("no kernel ({error})")),
    };
    let kernel = match runtime.load(id, &bytes) {
        Ok(kernel) => kernel,
        Err(error) => return KernelResult::Failed(format!("kernel does not load: {error}")),
    };
    match kernel.run(text.as_bytes(), lang) {
        RunOutcome::Ok { verdict, micros } => {
            info!(kernel = %wasm.display(), micros, decision = ?verdict.decision, "kernel ran");
            KernelResult::Verdict(verdict)
        }
        RunOutcome::Err(error) => KernelResult::Failed(format!("kernel error: {error}")),
    }
}

fn score_all(
    fixtures: &Path,
    kernels: &Path,
    markdown: Option<&Path>,
    json: Option<&Path>,
) -> Result<bool> {
    let fixtures = load_all(fixtures)?;
    let runtime = KernelRuntime::new(RuntimeConfig::default())?;
    let mut scores = Vec::new();
    for (index, fixture) in fixtures.iter().enumerate() {
        let wasm = kernels.join(format!("{}.wasm", fixture.name));
        let id = u64::try_from(index).unwrap_or(u64::MAX);
        let result = run_kernel(&runtime, id, &wasm, &fixture.text, fixture.lang());
        if let KernelResult::Failed(cause) = &result {
            warn!(fixture = %fixture.name, %cause, "no verdict");
        }
        scores.extend(score(fixture, &result));
    }
    let summary = Summary::of(&scores);
    let report = render_markdown(&fixtures, &scores);
    std::io::stdout()
        .write_all(report.as_bytes())
        .context("cannot write the report")?;
    if let Some(path) = markdown {
        std::fs::write(path, &report)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    if let Some(path) = json {
        std::fs::write(path, serde_json::to_string_pretty(&scores)?)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    info!(
        total = summary.total,
        exact = summary.exact,
        missing = summary.missing,
        wrong = summary.wrong,
        passes = summary.passes(),
        "scored"
    );
    Ok(summary.passes())
}

fn main() -> ExitCode {
    let _tracing = match telemetry::trace::init("analyst-accuracy") {
        Ok(guard) => guard,
        Err(error) => {
            let _ = writeln!(
                std::io::stderr(),
                "analyst-accuracy: cannot start logging: {error:#}"
            );
            return ExitCode::from(2);
        }
    };
    let cli = Cli::parse();
    let outcome = match &cli.command {
        Command::Inputs { out } => write_inputs(&cli.fixtures, out).map(|()| true),
        Command::Score {
            kernels,
            markdown,
            json,
        } => score_all(&cli.fixtures, kernels, markdown.as_deref(), json.as_deref()),
    };
    match outcome {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            error!(error = format!("{error:#}"), "analyst-accuracy failed");
            ExitCode::from(2)
        }
    }
}
