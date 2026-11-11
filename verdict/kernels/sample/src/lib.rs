//! Skeleton kernel: the structure of a kernel, with the lookup left out.
//!
//! The SDK and this crate assume no language and read nothing. What to look
//! for, and how to look it up, is the kernel author's decision for one company
//! and report, taken from that company's previous reports. See
//! `infra/charts/conductor/conductor/files/agents/analyst.md`.

use verdict_sdk::{export_kernel, Report, Verdict};

/// Looks the metrics up, verifies what it found, and decides.
///
/// A real kernel finds each metric, verifies it, falls back to a broader lookup
/// it also wrote when the verification fails, and abstains with the reason when
/// that fails too. This skeleton looks nothing up, so it abstains.
pub fn analyze(_report: &Report) -> Verdict {
    Verdict::abstain("no lookup defined")
}

export_kernel!(crate::analyze);
