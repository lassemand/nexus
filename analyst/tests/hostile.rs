//! Hostile and malformed kernels, written as WAT. One module behaves by the
//! first byte of its input, so the warm-up calls (empty text, the built-in
//! sample) pass and the hostile behaviour starts only on a real run.

use std::sync::Arc;
use std::time::{Duration, Instant};

use analyst::{
    KernelError, KernelRuntime, LoadError, RunOutcome, RuntimeConfig, MAX_INPUT_BYTES,
    MAX_MODULE_BYTES,
};
use verdict_sdk::{Decision, Lang};

const OK_JSON: &str = r#"{"decision":"abstain","reason":"ok","metrics":[]}"#;
const INPUT_AT: u32 = 65536;

fn behaving(extra_runs: &str) -> String {
    let len = OK_JSON.len();
    format!(
        r#"(module
  (memory (export "memory") 17)
  (data (i32.const 1024) "{json}")
  (func (export "verdict_input") (param i32) (result i32) (i32.const {input}))
  (func (export "verdict_run") (param i32 i32) (result i64)
    (local $first i32)
    (local.set $first (i32.load8_u (i32.const {input})))
    {extra_runs}
    (i64.or (i64.shl (i64.const 1024) (i64.const 32)) (i64.const {len}))))"#,
        json = OK_JSON.replace('"', "\\\""),
        input = INPUT_AT,
    )
}

fn hostile() -> Vec<u8> {
    let extra = r#"
    (if (i32.eq (local.get $first) (i32.const 88))
      (then (loop $spin (br $spin))))
    (if (i32.eq (local.get $first) (i32.const 71))
      (then (drop (memory.grow (i32.const 16384)))))
    (if (i32.eq (local.get $first) (i32.const 80))
      (then (return (i64.or (i64.shl (i64.const 4294967040) (i64.const 32)) (i64.const 10)))))
    (if (i32.eq (local.get $first) (i32.const 84))
      (then (return (i64.or (i64.shl (i64.const 0) (i64.const 32)) (i64.const 1048576)))))
    (if (i32.eq (local.get $first) (i32.const 74))
      (then (return (i64.or (i64.shl (i64.const 70000) (i64.const 32)) (i64.const 8)))))
    (if (i32.eq (local.get $first) (i32.const 85))
      (then unreachable))
    "#;
    wat::parse_str(behaving(extra)).expect("valid WAT")
}

fn runtime() -> KernelRuntime {
    KernelRuntime::new(RuntimeConfig::default()).expect("the engine starts")
}

fn assert_recovers(kernel: &analyst::LoadedKernel) {
    match kernel.run(b"hello", Lang::En) {
        RunOutcome::Ok { verdict, .. } => {
            assert_eq!(verdict.decision, Decision::Abstain);
            assert_eq!(verdict.reason, "ok");
        }
        RunOutcome::Err(error) => panic!("the kernel did not recover: {error}"),
    }
}

fn error_of(kernel: &analyst::LoadedKernel, text: &[u8]) -> KernelError {
    match kernel.run(text, Lang::En) {
        RunOutcome::Err(error) => error,
        RunOutcome::Ok { .. } => panic!("expected an error"),
    }
}

#[test]
fn a_behaving_module_runs_twice_with_identical_output() {
    let kernel = runtime().load(1, &hostile()).expect("loads");
    let first = kernel.run(b"hello", Lang::En);
    let second = kernel.run(b"hello", Lang::En);
    match (first, second) {
        (RunOutcome::Ok { verdict: a, .. }, RunOutcome::Ok { verdict: b, .. }) => {
            assert_eq!(a, b);
            assert_eq!(a.reason, "ok");
        }
        other => panic!("unexpected outcomes: {other:?}"),
    }
    assert_eq!(kernel.sha256().len(), 64);
}

#[test]
fn an_infinite_loop_times_out_within_60_ms_and_the_kernel_recovers() {
    let kernel = runtime().load(2, &hostile()).expect("loads");
    let started = Instant::now();
    let error = error_of(&kernel, b"Xspin");
    let elapsed = started.elapsed();
    assert_eq!(error, KernelError::Timeout);
    assert!(elapsed < Duration::from_millis(60), "took {elapsed:?}");
    assert_recovers(&kernel);
}

#[test]
fn growing_memory_to_one_gibibyte_traps_and_the_kernel_recovers() {
    let kernel = runtime().load(3, &hostile()).expect("loads");
    assert!(matches!(error_of(&kernel, b"Grow"), KernelError::Trap(_)));
    assert_recovers(&kernel);
}

#[test]
fn an_output_pointer_outside_memory_is_bad_output() {
    let kernel = runtime().load(4, &hostile()).expect("loads");
    assert!(matches!(
        error_of(&kernel, b"Ptr"),
        KernelError::BadOutput(_)
    ));
    assert_recovers(&kernel);
}

#[test]
fn a_one_mebibyte_output_is_too_large() {
    let kernel = runtime().load(5, &hostile()).expect("loads");
    assert_eq!(error_of(&kernel, b"Too big"), KernelError::TooLarge);
    assert_recovers(&kernel);
}

#[test]
fn invalid_json_is_bad_output() {
    let kernel = runtime().load(6, &hostile()).expect("loads");
    assert!(matches!(
        error_of(&kernel, b"Json"),
        KernelError::BadOutput(_)
    ));
    assert_recovers(&kernel);
}

#[test]
fn a_trap_is_reported_and_the_kernel_recovers() {
    let kernel = runtime().load(7, &hostile()).expect("loads");
    assert!(matches!(
        error_of(&kernel, b"Unreachable"),
        KernelError::Trap(_)
    ));
    assert_recovers(&kernel);
}

#[test]
fn every_error_in_a_row_leaves_a_working_kernel() {
    let kernel = runtime().load(8, &hostile()).expect("loads");
    for text in [
        &b"Xspin"[..],
        b"Grow",
        b"Ptr",
        b"Too big",
        b"Json",
        b"Unreachable",
    ] {
        assert!(matches!(kernel.run(text, Lang::En), RunOutcome::Err(_)));
        assert_recovers(&kernel);
    }
}

#[test]
fn input_over_one_mebibyte_is_refused_without_calling_the_kernel() {
    let kernel = runtime().load(9, &hostile()).expect("loads");
    let text = vec![b'X'; MAX_INPUT_BYTES + 1];
    let started = Instant::now();
    assert_eq!(error_of(&kernel, &text), KernelError::InputTooLarge);
    assert!(started.elapsed() < Duration::from_millis(40));
    assert_recovers(&kernel);
}

#[test]
fn a_module_with_imports_is_refused() {
    let wasm = wat::parse_str(
        r#"(module
  (import "env" "anything" (func))
  (memory (export "memory") 1)
  (func (export "verdict_input") (param i32) (result i32) (i32.const 0))
  (func (export "verdict_run") (param i32 i32) (result i64) (i64.const 0)))"#,
    )
    .expect("valid WAT");
    match runtime().load(10, &wasm) {
        Err(LoadError::Imports(name)) => assert_eq!(name, "env.anything"),
        other => panic!("expected Imports, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn a_missing_export_is_refused() {
    let wasm = wat::parse_str(
        r#"(module
  (memory (export "memory") 1)
  (func (export "verdict_input") (param i32) (result i32) (i32.const 0)))"#,
    )
    .expect("valid WAT");
    assert!(matches!(
        runtime().load(11, &wasm),
        Err(LoadError::Exports(_))
    ));
}

#[test]
fn wrong_export_signatures_are_refused() {
    let wrong_run = wat::parse_str(
        r#"(module
  (memory (export "memory") 1)
  (func (export "verdict_input") (param i32) (result i32) (i32.const 0))
  (func (export "verdict_run") (param i32) (result i64) (i64.const 0)))"#,
    )
    .expect("valid WAT");
    assert!(matches!(
        runtime().load(12, &wrong_run),
        Err(LoadError::Exports(_))
    ));
    let wrong_input = wat::parse_str(
        r#"(module
  (memory (export "memory") 1)
  (func (export "verdict_input") (param i64) (result i32) (i32.const 0))
  (func (export "verdict_run") (param i32 i32) (result i64) (i64.const 0)))"#,
    )
    .expect("valid WAT");
    assert!(matches!(
        runtime().load(13, &wrong_input),
        Err(LoadError::Exports(_))
    ));
    let no_memory = wat::parse_str(
        r#"(module
  (memory 1)
  (func (export "verdict_input") (param i32) (result i32) (i32.const 0))
  (func (export "verdict_run") (param i32 i32) (result i64) (i64.const 0)))"#,
    )
    .expect("valid WAT");
    assert!(matches!(
        runtime().load(14, &no_memory),
        Err(LoadError::Exports(_))
    ));
}

#[test]
fn extra_exports_are_ignored() {
    let wasm = wat::parse_str(
        r#"(module
  (memory (export "memory") 17)
  (data (i32.const 1024) "{\"decision\":\"abstain\",\"reason\":\"ok\",\"metrics\":[]}")
  (func (export "extra") (result i32) (i32.const 1))
  (func (export "verdict_input") (param i32) (result i32) (i32.const 65536))
  (func (export "verdict_run") (param i32 i32) (result i64)
    (i64.or (i64.shl (i64.const 1024) (i64.const 32)) (i64.const 49))))"#,
    )
    .expect("valid WAT");
    assert!(runtime().load(15, &wasm).is_ok());
}

#[test]
fn garbage_and_oversized_modules_are_refused() {
    let runtime = runtime();
    assert!(matches!(
        runtime.load(16, b"not wasm"),
        Err(LoadError::Invalid(_))
    ));
    let big = vec![0u8; MAX_MODULE_BYTES + 1];
    assert!(matches!(
        runtime.load(17, &big),
        Err(LoadError::TooLarge { .. })
    ));
}

#[test]
fn a_module_that_needs_more_than_the_memory_limit_is_refused() {
    let wasm = wat::parse_str(
        r#"(module
  (memory (export "memory") 2000)
  (func (export "verdict_input") (param i32) (result i32) (i32.const 0))
  (func (export "verdict_run") (param i32 i32) (result i64) (i64.const 0)))"#,
    )
    .expect("valid WAT");
    assert!(matches!(
        runtime().load(18, &wasm),
        Err(LoadError::Instantiate(_))
    ));
}

#[test]
fn a_kernel_that_traps_on_warm_up_is_refused() {
    let wasm = wat::parse_str(
        r#"(module
  (memory (export "memory") 1)
  (func (export "verdict_input") (param i32) (result i32) (i32.const 0))
  (func (export "verdict_run") (param i32 i32) (result i64) unreachable))"#,
    )
    .expect("valid WAT");
    assert!(matches!(runtime().load(19, &wasm), Err(LoadError::Warm(_))));
}

#[test]
fn two_kernels_run_concurrently_without_blocking_each_other() {
    let runtime = runtime();
    let slow = Arc::new(runtime.load(20, &hostile()).expect("loads"));
    let fast = Arc::new(runtime.load(21, &hostile()).expect("loads"));
    let spinning = {
        let slow = Arc::clone(&slow);
        std::thread::spawn(move || slow.run(b"Xspin", Lang::En))
    };
    std::thread::sleep(Duration::from_millis(5));
    let started = Instant::now();
    let quick = {
        let fast = Arc::clone(&fast);
        std::thread::spawn(move || {
            for _ in 0..200 {
                assert!(matches!(
                    fast.run(b"hello", Lang::En),
                    RunOutcome::Ok { .. }
                ));
            }
            started.elapsed()
        })
    };
    let quick_elapsed = quick.join().expect("the fast thread finishes");
    assert!(
        !spinning.is_finished(),
        "the slow kernel should still be spinning"
    );
    assert!(
        quick_elapsed < Duration::from_millis(40),
        "took {quick_elapsed:?}"
    );
    assert_eq!(
        spinning.join().expect("the slow thread finishes"),
        RunOutcome::Err(KernelError::Timeout)
    );
}

#[test]
fn a_configured_deadline_applies() {
    let config = RuntimeConfig {
        deadline: Duration::from_millis(10),
        ..RuntimeConfig::default()
    };
    let kernel = KernelRuntime::new(config)
        .expect("the engine starts")
        .load(22, &hostile())
        .expect("loads");
    let started = Instant::now();
    assert_eq!(error_of(&kernel, b"Xspin"), KernelError::Timeout);
    assert!(started.elapsed() < Duration::from_millis(30));
}

#[tokio::test]
async fn run_blocking_returns_the_verdict() {
    let kernel = Arc::new(runtime().load(23, &hostile()).expect("loads"));
    let outcome = analyst::run_blocking(kernel, b"hello".to_vec(), Lang::Sv).await;
    assert!(matches!(outcome, RunOutcome::Ok { .. }));
}
