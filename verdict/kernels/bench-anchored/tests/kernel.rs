//! The example company's last four reports in two languages, and a foreign release.

use verdict_bench_anchored::analyze;
use verdict_sdk::harness::{assert_abstains, check_fixtures};
use verdict_sdk::{Decision, Lang, Report};

#[path = "../../bench-common/fixtures.rs"]
mod fixtures;

#[test]
fn fixtures_give_the_expected_metrics_and_decisions() {
    if let Err(diff) = check_fixtures(analyze, &fixtures::fixtures()) {
        panic!("{diff}");
    }
}

#[test]
fn a_foreign_release_abstains() {
    assert_abstains(analyze, fixtures::FOREIGN, Lang::En);
    assert_abstains(analyze, fixtures::FOREIGN, Lang::No);
}

#[test]
fn falls_back_to_the_window_when_a_paragraph_prefix_changes() {
    let all = fixtures::fixtures();
    let original = &all[0];
    let changed = original.text.replace(
        "\n\nOperational EBIT in Farming was",
        "\n\nIn short, Operational EBIT in Farming was",
    );
    assert_ne!(changed, original.text);
    let expected = analyze(&Report {
        text: original.text,
        lang: original.lang,
    });
    let actual = analyze(&Report {
        text: &changed,
        lang: original.lang,
    });
    assert_eq!(actual, expected);
}

#[test]
fn abstains_when_the_label_is_gone_from_both_lookups() {
    let all = fixtures::fixtures();
    let original = &all[0];
    let renamed = original
        .text
        .replace("Operational EBIT in Farming", "Operating profit in Farming");
    let verdict = analyze(&Report {
        text: &renamed,
        lang: original.lang,
    });
    assert_eq!(verdict.decision, Decision::Abstain);
    assert_eq!(verdict.reason, "farming EBIT not found");
    assert!(verdict
        .metrics
        .iter()
        .all(|metric| metric.key != "farming_ebit"));
}
