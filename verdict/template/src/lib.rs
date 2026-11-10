//! Kernel for one company's report. Rules: `verdict/AGENT_GUIDE.md`.
//!
//! Write the lookup for this company's layout, using the cheapest rung of the
//! guide's lookup ladder that its previous reports prove stable. Verify what
//! the lookup found, fall back to a broader lookup you also wrote, otherwise
//! abstain. Replace the decision rule.

use verdict_sdk::{export_kernel, Report, Verdict};

/// Metric keys this kernel returns.
pub const KEYS: [&str; 2] = ["net_sales", "ebit"];

/// Reads the metrics and decides.
pub fn analyze(_report: &Report) -> Verdict {
    Verdict::abstain("kernel not written")
}

export_kernel!(crate::analyze);
