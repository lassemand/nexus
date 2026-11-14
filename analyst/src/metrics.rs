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

/// Histogram: seconds from consuming a release to its decision being handed on.
pub const RELEASE_TO_DECISION_SECONDS: &str = "analyst_release_to_decision_seconds";
/// Histogram: seconds from MFN publishing a release to its decision being handed on.
/// Includes MFN's and `nordic-live`'s delay: for dashboards, not alerts.
pub const PUBLISH_TO_DECISION_SECONDS: &str = "analyst_publish_to_decision_seconds";
/// Counter: release events, labelled `outcome` (`decided`, `duplicate`, `ignored_<reason>`).
pub const RELEASE_EVENTS_TOTAL: &str = "analyst_release_events_total";
/// Counter: decisions recorded, labelled `decision`.
pub const DECISIONS_TOTAL: &str = "analyst_decisions_total";
/// Counter: retried attempts to write a decision or to emit it, labelled `step`.
pub const DECISION_RETRIES_TOTAL: &str = "analyst_decision_retries_total";

/// Bucket bounds (seconds) for [`RELEASE_TO_DECISION_SECONDS`]: 100 µs to 5 s.
pub const RELEASE_BUCKETS: &[f64] = &[
    1e-4, 2e-4, 5e-4, 1e-3, 2e-3, 5e-3, 1e-2, 2e-2, 5e-2, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
];
/// Bucket bounds (seconds) for [`PUBLISH_TO_DECISION_SECONDS`]: 0.5 s to 30 min.
pub const PUBLISH_BUCKETS: &[f64] = &[0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 300.0, 900.0, 1800.0];

/// Describes the release path's metrics.
pub fn describe_releases() {
    metrics::describe_histogram!(
        RELEASE_TO_DECISION_SECONDS,
        "Seconds from consuming a release to its decision being handed on"
    );
    metrics::describe_histogram!(
        PUBLISH_TO_DECISION_SECONDS,
        "Seconds from MFN publishing a release to its decision being handed on"
    );
    metrics::describe_counter!(RELEASE_EVENTS_TOTAL, "Release events by outcome");
    metrics::describe_counter!(DECISIONS_TOTAL, "Decisions recorded by decision");
    metrics::describe_counter!(
        DECISION_RETRIES_TOTAL,
        "Retried attempts to write or emit a decision, by step"
    );
}

pub(crate) fn release_event(outcome: &str) {
    metrics::counter!(RELEASE_EVENTS_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

pub(crate) fn decision(decision: &'static str) {
    metrics::counter!(DECISIONS_TOTAL, "decision" => decision).increment(1);
}

pub(crate) fn decision_retry(step: &'static str) {
    metrics::counter!(DECISION_RETRIES_TOTAL, "step" => step).increment(1);
}

pub(crate) fn release_to_decision(elapsed: Duration) {
    metrics::histogram!(RELEASE_TO_DECISION_SECONDS).record(elapsed.as_secs_f64());
}

pub(crate) fn publish_to_decision(seconds: f64) {
    metrics::histogram!(PUBLISH_TO_DECISION_SECONDS).record(seconds.max(0.0));
}

/// Seconds from a decision to its entry quote, for decisions taken in session.
pub const ENTRY_DELAY_SECONDS: &str = "analyst_entry_delay_seconds";
/// Seconds a scheduled quote was fetched after its target.
pub const QUOTE_LATENESS_SECONDS: &str = "analyst_quote_lateness_seconds";
/// Simulated trades by final status and shadow flag.
pub const SIM_TRADES_TOTAL: &str = "analyst_sim_trades_total";
/// Quote requests by outcome.
pub const QUOTES_TOTAL: &str = "analyst_quotes_total";

/// Bucket bounds (seconds) for [`ENTRY_DELAY_SECONDS`]: 1 ms to 5 min.
pub const ENTRY_DELAY_BUCKETS: &[f64] = &[
    1e-3, 5e-3, 1e-2, 5e-2, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 300.0,
];
/// Bucket bounds (seconds) for [`QUOTE_LATENESS_SECONDS`]: 1 ms to 30 s.
pub const LATENESS_BUCKETS: &[f64] = &[1e-3, 5e-3, 1e-2, 5e-2, 0.1, 0.5, 1.0, 2.0, 5.0, 30.0];

/// Describes the simulated trades' metrics.
pub fn describe_trades() {
    metrics::describe_histogram!(
        ENTRY_DELAY_SECONDS,
        "Seconds from a decision taken in session to its accepted entry quote"
    );
    metrics::describe_histogram!(
        QUOTE_LATENESS_SECONDS,
        "Seconds a scheduled quote was fetched after its target"
    );
    metrics::describe_counter!(SIM_TRADES_TOTAL, "Simulated trades by status and shadow");
    metrics::describe_counter!(QUOTES_TOTAL, "Quote requests by outcome");
}

/// Records the delay from a decision taken in session to its entry quote.
pub fn entry_delay(elapsed: Duration) {
    metrics::histogram!(ENTRY_DELAY_SECONDS).record(elapsed.as_secs_f64());
}

/// Records how late a scheduled quote was fetched.
pub fn quote_lateness(seconds: f64) {
    metrics::histogram!(QUOTE_LATENESS_SECONDS).record(seconds.max(0.0));
}

/// Counts a simulated trade that reached `status`.
pub fn sim_trade(status: &'static str, shadow: bool) {
    metrics::counter!(SIM_TRADES_TOTAL, "status" => status, "shadow" => shadow.to_string())
        .increment(1);
}

/// Counts a quote request and how it went (`ok`, `accepted`, `rejected`, or an error outcome).
pub fn quote_request(outcome: &'static str) {
    metrics::counter!(QUOTES_TOTAL, "outcome" => outcome).increment(1);
}
