//! Kernel for one company's report. Rules: `verdict/AGENT_GUIDE.md`.
//!
//! Replace the phrases with the company's own wording from its previous
//! reports, in every language it publishes in, and replace the decision rule.

use std::sync::LazyLock;

use verdict_sdk::{
    current_and_prior, export_kernel, unit_after, Decision, Labels, Metric, Report, Verdict,
};

/// Metric keys, in the order of [`PHRASES`].
pub const KEYS: [&str; 2] = ["net_sales", "ebit"];

/// Label phrases per metric. A trailing `*` is an open stem.
pub const PHRASES: [&[&str]; 2] = [
    &["net sales", "nettoomsättning*"],
    &["operating profit", "ebit", "rörelseresultat*"],
];

static LABELS: LazyLock<Labels> = LazyLock::new(|| Labels::new(&PHRASES));

const WINDOW: usize = 200;

/// Reads the metrics and decides.
pub fn analyze(report: &Report) -> Verdict {
    let text = report.text;
    let style = report.lang.number_style();
    let metrics: Vec<Metric> = KEYS
        .iter()
        .zip(LABELS.first_hits(text))
        .filter_map(|(key, hit)| {
            let hit = hit?;
            let (current, prior) = current_and_prior(text, hit.end, WINDOW, style)?;
            Some(Metric {
                key,
                current: Some(current.value.to_string()),
                prior: prior.map(|prior| prior.value.to_string()),
                expected: None,
                unit: unit_after(text, current.end).map(str::to_owned),
            })
        })
        .collect();
    let reason = if metrics.len() < KEYS.len() {
        "metrics missing"
    } else {
        "decision rule not written"
    };
    Verdict {
        decision: Decision::Abstain,
        reason: reason.to_owned(),
        metrics,
    }
}

export_kernel!(crate::analyze);
