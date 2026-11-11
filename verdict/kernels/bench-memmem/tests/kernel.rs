//! The example company's last four reports in two languages, and a foreign release.

use verdict_bench_memmem::analyze;
use verdict_sdk::harness::{assert_abstains, check_fixtures};
use verdict_sdk::Lang;

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
