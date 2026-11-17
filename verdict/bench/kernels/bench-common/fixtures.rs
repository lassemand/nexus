//! The example company's last four quarterly reports, in English and
//! Norwegian, with the values read by hand from the texts. Shared by the bench
//! kernels' tests and the analyst's tests through `#[path]`.

use verdict_sdk::harness::Fixture;
use verdict_sdk::{Decision, Lang};

type Expected = Vec<(&'static str, Option<&'static str>, Option<&'static str>)>;

fn expected(rows: [(&'static str, &'static str, Option<&'static str>); 3]) -> Expected {
    rows.into_iter()
        .map(|(key, current, prior)| (key, Some(current), prior))
        .collect()
}

/// One release per language and quarter, newest first.
pub fn fixtures() -> Vec<Fixture<'static>> {
    let q3 = || {
        expected([
            ("group_ebit", "459", Some("15")),
            ("farming_ebit", "113", Some("-306")),
            ("wild_catch_ebit", "-20", Some("3")),
        ])
    };
    let q2 = || {
        expected([
            ("group_ebit", "574", Some("680")),
            ("farming_ebit", "236", Some("256")),
            ("wild_catch_ebit", "140", Some("148")),
        ])
    };
    let q1 = || {
        expected([
            ("group_ebit", "858", Some("1049")),
            ("farming_ebit", "555", Some("789")),
            ("wild_catch_ebit", "228", Some("148")),
        ])
    };
    let q4 = || {
        expected([
            ("group_ebit", "758", None),
            ("farming_ebit", "564", None),
            ("wild_catch_ebit", "-29", Some("6")),
        ])
    };
    let row = |name, text, lang, expected, expected_decision| Fixture {
        name,
        text,
        lang,
        expected,
        expected_decision,
    };
    vec![
        row(
            "en__lsg__2026-11-10",
            include_str!("../bench-fixtures/lsg/en__lsg__2026-11-10.txt"),
            Lang::En,
            q3(),
            Decision::Buy,
        ),
        row(
            "no__lsg__2026-11-10",
            include_str!("../bench-fixtures/lsg/no__lsg__2026-11-10.txt"),
            Lang::No,
            q3(),
            Decision::Buy,
        ),
        row(
            "en__lsg__2026-08-19",
            include_str!("../bench-fixtures/lsg/en__lsg__2026-08-19.txt"),
            Lang::En,
            q2(),
            Decision::NoBuy,
        ),
        row(
            "no__lsg__2026-08-19",
            include_str!("../bench-fixtures/lsg/no__lsg__2026-08-19.txt"),
            Lang::No,
            q2(),
            Decision::NoBuy,
        ),
        row(
            "en__lsg__2026-05-12",
            include_str!("../bench-fixtures/lsg/en__lsg__2026-05-12.txt"),
            Lang::En,
            q1(),
            Decision::NoBuy,
        ),
        row(
            "no__lsg__2026-05-12",
            include_str!("../bench-fixtures/lsg/no__lsg__2026-05-12.txt"),
            Lang::No,
            q1(),
            Decision::NoBuy,
        ),
        row(
            "en__lsg__2026-02-24",
            include_str!("../bench-fixtures/lsg/en__lsg__2026-02-24.txt"),
            Lang::En,
            q4(),
            Decision::Abstain,
        ),
        row(
            "no__lsg__2026-02-24",
            include_str!("../bench-fixtures/lsg/no__lsg__2026-02-24.txt"),
            Lang::No,
            q4(),
            Decision::Abstain,
        ),
    ]
}

/// Another company's interim report; every bench kernel must abstain on it.
pub const FOREIGN: &str = "Other Company Oyj: Interim report January–September 2026\n\n\
    Revenue increased to EUR 41.2 million (38.0). Operating profit was EUR 3.1 million (2.4).\n\
    Operational activity continued in line with plan.";
