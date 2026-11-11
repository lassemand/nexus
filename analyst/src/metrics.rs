//! Metric names and recording helpers for the kernel runtime.
//!
//! The `metrics` macros do nothing until a recorder is installed (the service
//! installs one with `telemetry`), so the runtime and its tests need none.

use std::time::Duration;

use crate::KernelError;

/// Histogram: seconds per kernel run, successful or not.
pub const RUN_SECONDS: &str = "analyst_kernel_run_seconds";
/// Counter: failed runs, labelled `kind`.
pub const ERRORS_TOTAL: &str = "analyst_kernel_errors_total";
/// Histogram: seconds per load step, labelled `step` (`validate`, `compile`,
/// `instantiate`, `warm`).
pub const LOAD_SECONDS: &str = "analyst_kernel_load_seconds";
/// Gauge: kernels currently loaded.
pub const KERNELS_LOADED: &str = "analyst_kernels_loaded";

/// Bucket bounds (seconds) for [`RUN_SECONDS`]: from 1 µs to the 50 ms deadline and beyond.
pub const RUN_BUCKETS: &[f64] = &[
    1e-6, 2e-6, 5e-6, 1e-5, 2e-5, 5e-5, 1e-4, 2e-4, 5e-4, 1e-3, 5e-3, 2e-2, 5e-2, 0.1,
];
/// Bucket bounds (seconds) for [`LOAD_SECONDS`].
pub const LOAD_BUCKETS: &[f64] = &[1e-4, 1e-3, 5e-3, 2e-2, 5e-2, 0.1, 0.25, 0.5, 1.0, 2.5];

pub(crate) fn run(elapsed: Duration, result: &Result<(), &KernelError>) {
    metrics::histogram!(RUN_SECONDS).record(elapsed.as_secs_f64());
    if let Err(error) = result {
        metrics::counter!(ERRORS_TOTAL, "kind" => error.kind()).increment(1);
    }
}

pub(crate) fn load_step(step: &'static str, elapsed: Duration) {
    metrics::histogram!(LOAD_SECONDS, "step" => step).record(elapsed.as_secs_f64());
}

pub(crate) fn loaded(delta: f64) {
    if delta >= 0.0 {
        metrics::gauge!(KERNELS_LOADED).increment(delta);
    } else {
        metrics::gauge!(KERNELS_LOADED).decrement(-delta);
    }
}

/// Gauge: report targets, labelled `status`.
pub const TARGETS: &str = "analyst_targets";
/// Counter: preparations started.
pub const PREP_STARTED_TOTAL: &str = "analyst_prep_started_total";
/// Counter: failed preparations, labelled `reason` (`launch`, `failed`, `timed_out`, `rejected`).
pub const PREP_FAILED_TOTAL: &str = "analyst_prep_failed_total";
/// Gauge: seconds until the soonest report that has no ready kernel; -1 when there is none.
pub const NEXT_REPORT_SECONDS: &str = "analyst_next_report_seconds";
/// Counter: notes consumed, labelled `outcome` (`stored`, `duplicate`, `rejected_unregistered`,
/// `rejected_too_large`, `rejected_invalid`).
pub const NOTES_TOTAL: &str = "analyst_notes_total";

/// Describes the scheduler's and the notes consumer's metrics.
pub fn describe() {
    metrics::describe_gauge!(TARGETS, "Report targets by status");
    metrics::describe_counter!(PREP_STARTED_TOTAL, "Kernel preparations started");
    metrics::describe_counter!(
        PREP_FAILED_TOTAL,
        "Kernel preparations that failed, by reason"
    );
    metrics::describe_gauge!(
        NEXT_REPORT_SECONDS,
        "Seconds until the soonest report with no ready kernel (-1 when none)"
    );
    metrics::describe_counter!(NOTES_TOTAL, "Report notes consumed, by outcome");
}

pub(crate) fn targets(status: &'static str, count: i64) {
    metrics::gauge!(TARGETS, "status" => status).set(count as f64);
}

pub(crate) fn prep_started() {
    metrics::counter!(PREP_STARTED_TOTAL).increment(1);
}

pub(crate) fn prep_failed(reason: &'static str) {
    metrics::counter!(PREP_FAILED_TOTAL, "reason" => reason).increment(1);
}

pub(crate) fn next_report(seconds: f64) {
    metrics::gauge!(NEXT_REPORT_SECONDS).set(seconds);
}

pub(crate) fn note(outcome: &'static str) {
    metrics::counter!(NOTES_TOTAL, "outcome" => outcome).increment(1);
}
