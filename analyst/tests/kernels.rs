//! The bench kernels and the skeleton sample, built for `wasm32-unknown-unknown`
//! in the test, run through `KernelRuntime` and compared with native.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use analyst::{KernelRuntime, RunOutcome, RuntimeConfig};
use verdict_sdk::{run_into, Analyze, Decision, Lang};

#[path = "../../verdict/kernels/bench-common/fixtures.rs"]
mod fixtures;

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn wasm_path(package: &str) -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace().join("target"));
    target
        .join("wasm32-unknown-unknown/release")
        .join(format!("{}.wasm", package.replace('-', "_")))
}

fn build_wasm() {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| {
        let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .current_dir(workspace())
            .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
            .args(["-p", "verdict-bench-anchored", "-p", "verdict-bench-memmem"])
            .args(["-p", "verdict-bench-aho", "-p", "verdict-sample"])
            .status()
            .expect("cargo runs");
        assert!(status.success(), "building the kernels for wasm32 failed");
    });
}

fn wasm(package: &str) -> Vec<u8> {
    build_wasm();
    let path = wasm_path(package);
    std::fs::read(&path).unwrap_or_else(|error| panic!("reading {path:?}: {error}"))
}

fn runtime() -> KernelRuntime {
    KernelRuntime::new(RuntimeConfig::default()).expect("the engine starts")
}

fn native_json(analyze: Analyze, text: &str, lang: Lang) -> Vec<u8> {
    let mut out = Vec::new();
    run_into(analyze, text.as_bytes(), lang.code(), &mut out);
    out
}

fn assert_byte_identical(package: &str, analyze: Analyze) {
    let kernel = runtime().load(1, &wasm(package)).expect("the kernel loads");
    for fixture in fixtures::fixtures() {
        let (wasm_json, _) = kernel
            .run_with(fixture.text.as_bytes(), fixture.lang, |bytes| {
                Ok(bytes.to_vec())
            })
            .expect("the kernel runs");
        let native = native_json(analyze, fixture.text, fixture.lang);
        assert_eq!(
            String::from_utf8_lossy(&wasm_json),
            String::from_utf8_lossy(&native),
            "{package} on {}",
            fixture.name
        );
    }
}

#[test]
fn bench_anchored_wasm_equals_native_in_both_languages() {
    assert_byte_identical("verdict-bench-anchored", verdict_bench_anchored::analyze);
}

#[test]
fn bench_memmem_wasm_equals_native_in_both_languages() {
    assert_byte_identical("verdict-bench-memmem", verdict_bench_memmem::analyze);
}

#[test]
fn bench_aho_wasm_equals_native_in_both_languages() {
    assert_byte_identical("verdict-bench-aho", verdict_bench_aho::analyze);
}

#[test]
fn wasm_verdicts_are_the_expected_decisions() {
    let kernel = runtime()
        .load(2, &wasm("verdict-bench-anchored"))
        .expect("the kernel loads");
    for fixture in fixtures::fixtures() {
        match kernel.run(fixture.text.as_bytes(), fixture.lang) {
            RunOutcome::Ok { verdict, .. } => assert_eq!(
                verdict.decision, fixture.expected_decision,
                "{}",
                fixture.name
            ),
            RunOutcome::Err(error) => panic!("{}: {error}", fixture.name),
        }
    }
}

#[test]
fn a_second_run_on_the_same_instance_gives_identical_output() {
    let kernel = runtime()
        .load(3, &wasm("verdict-bench-anchored"))
        .expect("the kernel loads");
    let text = fixtures::fixtures()[0].text.as_bytes().to_vec();
    let first = kernel
        .run_with(&text, Lang::En, |bytes| Ok(bytes.to_vec()))
        .expect("runs")
        .0;
    for _ in 0..5 {
        let again = kernel
            .run_with(&text, Lang::En, |bytes| Ok(bytes.to_vec()))
            .expect("runs")
            .0;
        assert_eq!(again, first);
    }
}

#[test]
fn the_skeleton_sample_loads_and_abstains() {
    let kernel = runtime()
        .load(4, &wasm("verdict-sample"))
        .expect("the kernel loads");
    match kernel.run(b"any release text", Lang::Other) {
        RunOutcome::Ok { verdict, .. } => {
            assert_eq!(verdict.decision, Decision::Abstain);
            assert_eq!(verdict.reason, "no lookup defined");
            assert!(verdict.metrics.is_empty());
        }
        RunOutcome::Err(error) => panic!("{error}"),
    }
}

#[test]
fn a_foreign_release_abstains_in_wasm() {
    let kernel = runtime()
        .load(5, &wasm("verdict-bench-aho"))
        .expect("the kernel loads");
    match kernel.run(fixtures::FOREIGN.as_bytes(), Lang::En) {
        RunOutcome::Ok { verdict, .. } => assert_eq!(verdict.decision, Decision::Abstain),
        RunOutcome::Err(error) => panic!("{error}"),
    }
}
