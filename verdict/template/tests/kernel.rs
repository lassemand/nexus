//! Asserts the kernel on the company's previous releases in `tests/fixtures/`.
//!
//! Save each release as `tests/fixtures/<lang>__<slug>__<id>.txt` (title, blank
//! line, `content.text`) and add a [`Case`] to [`CASES`] with every metric's
//! current and prior value: the last two reports in each language the company
//! publishes in. Keep [`FOREIGN`] a release from another company.

use verdict_kernel::analyze;
use verdict_sdk::harness::{assert_abstains, assert_fixtures, time_per_release, Fixture};
use verdict_sdk::{Decision, Lang};

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

fn read_fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("reading {path}: {err}"))
}

fn with_fixtures(check: impl FnOnce(&[Fixture])) {
    let texts: Vec<String> = CASES
        .iter()
        .map(|case| read_fixture(case.fixture))
        .collect();
    let fixtures: Vec<Fixture> = CASES
        .iter()
        .zip(&texts)
        .map(|(case, text)| Fixture {
            name: case.fixture,
            text,
            lang: case.lang,
            expected: case.metrics.to_vec(),
            expected_decision: case.decision,
        })
        .collect();
    check(&fixtures);
}

#[test]
fn fixtures_give_expected_metrics() {
    with_fixtures(|fixtures| assert_fixtures(analyze, fixtures));
}

#[test]
fn foreign_release_abstains() {
    assert_abstains(analyze, FOREIGN, Lang::En);
}

#[test]
#[ignore = "run with --ignored --nocapture and record p50/p99 in kernel.json"]
fn timing() {
    with_fixtures(|fixtures| {
        let (p50, p99) = time_per_release(analyze, fixtures, 1000);
        println!("p50 {p50:?} p99 {p99:?}");
    });
}
