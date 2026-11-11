//! Test and timing harness for kernels. Enable the `harness` feature in the
//! kernel's `[dev-dependencies]`; it is never part of a WASM build.

use std::fmt::Write;
use std::hint::black_box;
use std::time::{Duration, Instant};

use crate::{Analyze, Decision, Lang, Report};

/// One release and what the kernel must read from it.
#[derive(Debug, Clone)]
pub struct Fixture<'a> {
    /// A name shown in diffs, e.g. the fixture's file name.
    pub name: &'a str,
    /// The release text.
    pub text: &'a str,
    /// The release's language.
    pub lang: Lang,
    /// Every metric the kernel must return, as `(key, current, prior)`. No
    /// other metric may be returned.
    pub expected: Vec<(&'a str, Option<&'a str>, Option<&'a str>)>,
    /// The decision the kernel must return.
    pub expected_decision: Decision,
}

fn shown(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("\"{value}\""),
        None => "none".to_owned(),
    }
}

/// Runs `analyze` on every fixture and compares every value exactly.
///
/// # Errors
///
/// Returns a readable diff listing each wrong, missing or unexpected metric
/// value and a wrong decision, per fixture.
pub fn check_fixtures(analyze: Analyze, fixtures: &[Fixture]) -> Result<(), String> {
    let mut diff = String::new();
    for fixture in fixtures {
        let verdict = analyze(&Report {
            text: fixture.text,
            lang: fixture.lang,
        });
        let mut lines = String::new();
        for (key, current, prior) in &fixture.expected {
            let Some(found) = verdict.metrics.iter().find(|metric| metric.key == *key) else {
                let _ = writeln!(lines, "  {key}: missing");
                continue;
            };
            if found.current.as_deref() != *current {
                let _ = writeln!(
                    lines,
                    "  {key} current: expected {}, got {}",
                    shown(*current),
                    shown(found.current.as_deref())
                );
            }
            if found.prior.as_deref() != *prior {
                let _ = writeln!(
                    lines,
                    "  {key} prior: expected {}, got {}",
                    shown(*prior),
                    shown(found.prior.as_deref())
                );
            }
        }
        for metric in &verdict.metrics {
            if !fixture
                .expected
                .iter()
                .any(|(key, _, _)| *key == metric.key)
            {
                let _ = writeln!(lines, "  {}: unexpected metric", metric.key);
            }
        }
        if verdict.decision != fixture.expected_decision {
            let _ = writeln!(
                lines,
                "  decision: expected {:?}, got {:?} ({})",
                fixture.expected_decision, verdict.decision, verdict.reason
            );
        }
        if !lines.is_empty() {
            let _ = writeln!(diff, "{}:\n{lines}", fixture.name);
        }
    }
    if diff.is_empty() {
        Ok(())
    } else {
        Err(diff)
    }
}

/// Like [`check_fixtures`] but panics with the diff.
///
/// # Panics
///
/// Panics when any fixture's metrics or decision differ.
pub fn assert_fixtures(analyze: Analyze, fixtures: &[Fixture]) {
    if let Err(diff) = check_fixtures(analyze, fixtures) {
        panic!("fixtures differ:\n{diff}");
    }
}

/// The median and 99th percentile of one `analyze` call over every fixture.
///
/// Each fixture is run `iters` times after one warm-up call, so the statics a
/// lookup builds are not counted. Returns zero durations for no samples.
pub fn time_per_release(
    analyze: Analyze,
    fixtures: &[Fixture],
    iters: usize,
) -> (Duration, Duration) {
    let mut samples = Vec::with_capacity(fixtures.len().saturating_mul(iters));
    for fixture in fixtures {
        let report = Report {
            text: fixture.text,
            lang: fixture.lang,
        };
        black_box(analyze(black_box(&report)));
        for _ in 0..iters {
            let start = Instant::now();
            black_box(analyze(black_box(&report)));
            samples.push(start.elapsed());
        }
    }
    samples.sort_unstable();
    let at = |per_mille: usize| {
        let last = samples.len().saturating_sub(1);
        let index = (last * per_mille).div_ceil(1000);
        samples.get(index).copied().unwrap_or_default()
    };
    (at(500), at(990))
}

/// Asserts that `analyze` abstains on another company's report.
///
/// # Panics
///
/// Panics, with the metrics found, when the decision is not `Abstain`.
pub fn assert_abstains(analyze: Analyze, foreign_text: &str, lang: Lang) {
    let verdict = analyze(&Report {
        text: foreign_text,
        lang,
    });
    assert_eq!(
        verdict.decision,
        Decision::Abstain,
        "a foreign report must abstain; got {:?} ({}) with metrics {:?}",
        verdict.decision,
        verdict.reason,
        verdict.metrics
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Metric, Verdict};

    fn kernel(report: &Report) -> Verdict {
        let mut metric = Metric::new("net_sales");
        metric.current = Some(report.text.to_owned());
        metric.prior = Some("1.0".to_owned());
        Verdict {
            decision: Decision::Buy,
            reason: "always".to_owned(),
            metrics: vec![metric],
        }
    }

    fn fixture<'a>(text: &'a str, current: &'a str, decision: Decision) -> Fixture<'a> {
        Fixture {
            name: "f.txt",
            text,
            lang: Lang::En,
            expected: vec![("net_sales", Some(current), Some("1.0"))],
            expected_decision: decision,
        }
    }

    #[test]
    fn matching_fixture_passes() {
        assert_eq!(
            check_fixtures(kernel, &[fixture("2.5", "2.5", Decision::Buy)]),
            Ok(())
        );
    }

    #[test]
    fn wrong_value_gives_a_readable_diff() {
        let diff = check_fixtures(kernel, &[fixture("2.5", "2.6", Decision::Buy)])
            .expect_err("the value differs");
        assert_eq!(
            diff,
            "f.txt:\n  net_sales current: expected \"2.6\", got \"2.5\"\n\n"
        );
    }

    #[test]
    fn wrong_decision_missing_and_unexpected_metrics_are_reported() {
        let mut wrong = fixture("2.5", "2.5", Decision::NoBuy);
        wrong.expected.push(("ebit", Some("1"), None));
        wrong.expected.retain(|(key, _, _)| *key != "net_sales");
        let diff = check_fixtures(kernel, &[wrong]).expect_err("everything differs");
        assert!(diff.contains("ebit: missing"), "{diff}");
        assert!(diff.contains("net_sales: unexpected metric"), "{diff}");
        assert!(
            diff.contains("decision: expected NoBuy, got Buy (always)"),
            "{diff}"
        );
    }

    #[test]
    fn timing_reports_ordered_percentiles() {
        let fixtures = [fixture("2.5", "2.5", Decision::Buy)];
        let (p50, p99) = time_per_release(kernel, &fixtures, 200);
        assert!(p50 <= p99);
        assert_eq!(
            time_per_release(kernel, &[], 10),
            (Duration::ZERO, Duration::ZERO)
        );
    }

    #[test]
    fn abstains_passes_and_fails() {
        fn abstainer(_: &Report) -> Verdict {
            Verdict::abstain("not mine")
        }
        assert_abstains(abstainer, "x", Lang::En);
        let caught = std::panic::catch_unwind(|| assert_abstains(kernel, "x", Lang::En));
        assert!(caught.is_err());
    }
}
