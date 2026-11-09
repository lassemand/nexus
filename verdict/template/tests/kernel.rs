//! Asserts the kernel on the company's previous releases in `tests/fixtures/`.
//!
//! Save each release as `tests/fixtures/<lang>__<slug>__<id>.txt` (title, blank
//! line, `content.text`) and add a [`Case`] to [`CASES`] with every metric's
//! current and prior value: the last two reports in each language the company
//! publishes in. Keep [`FOREIGN`] a release from another company.

use verdict_kernel::analyze;
use verdict_sdk::{Decision, Lang, Report, Verdict};

/// One fixture and what the kernel must read from it.
struct Case {
    fixture: &'static str,
    lang: Lang,
    metrics: &'static [(&'static str, Option<&'static str>, Option<&'static str>)],
    decision: Decision,
}

/// Every fixture with its expected metrics `(key, current, prior)`.
const CASES: &[Case] = &[];

/// A release from another company; the kernel must abstain on it.
const FOREIGN: &str = "Other Company Oyj: Interim report January–September 2026\n\n\
    Revenue increased to EUR 41.2 million (38.0). Operating profit was EUR 3.1 million (2.4).";

fn run(text: &str, lang: Lang) -> Verdict {
    analyze(&Report { text, lang })
}

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("reading {path}: {err}"))
}

#[test]
fn fixtures_give_expected_metrics() {
    for case in CASES {
        let text = fixture(case.fixture);
        let verdict = run(&text, case.lang);
        for (key, current, prior) in case.metrics {
            let metric = verdict
                .metrics
                .iter()
                .find(|metric| metric.key == *key)
                .unwrap_or_else(|| panic!("{}: {key} not found", case.fixture));
            assert_eq!(
                metric.current.as_deref(),
                *current,
                "{}: {key} current",
                case.fixture
            );
            assert_eq!(
                metric.prior.as_deref(),
                *prior,
                "{}: {key} prior",
                case.fixture
            );
        }
        assert_eq!(
            verdict.metrics.len(),
            case.metrics.len(),
            "{}: metric count",
            case.fixture
        );
        assert_eq!(
            verdict.decision, case.decision,
            "{}: decision",
            case.fixture
        );
    }
}

#[test]
fn foreign_release_abstains() {
    assert_eq!(run(FOREIGN, Lang::En).decision, Decision::Abstain);
}
