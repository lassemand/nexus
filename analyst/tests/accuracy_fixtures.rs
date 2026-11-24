//! The extraction accuracy fixtures (`analyst/tests/accuracy/`): the set covers
//! what the validation needs, every hand-checked figure is printed where its
//! fixture says, and a kernel's output is scored through the real runtime.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::str::FromStr;

use analyst::accuracy::{
    coverage, line_shows, load_all, score, Fixture, Grade, KernelResult, Summary,
};
use analyst::{KernelRuntime, RunOutcome, RuntimeConfig};
use rust_decimal::Decimal;

const SECTORS: [&str; 4] = ["industrial", "bank", "property", "biotech"];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/accuracy")
}

fn fixtures() -> Vec<Fixture> {
    load_all(&root()).expect("every fixture loads")
}

fn own_language(country: &str) -> &'static str {
    match country {
        "SE" => "sv",
        "DK" => "da",
        "FI" => "fi",
        other => panic!("unexpected country {other}"),
    }
}

#[test]
fn the_set_covers_three_countries_both_languages_and_four_sectors() {
    let fixtures = fixtures();
    assert!(fixtures.len() >= 24, "{} fixtures, need 24", fixtures.len());
    let coverage = coverage(&fixtures);
    for country in ["SE", "DK", "FI"] {
        let count = coverage
            .per_country
            .iter()
            .find(|(c, _)| c == country)
            .map_or(0, |(_, n)| *n);
        assert!(count >= 8, "{country}: {count} fixtures, need 8");
        let langs = &coverage
            .languages
            .iter()
            .find(|(c, _)| c == country)
            .expect("the country has fixtures")
            .1;
        assert!(langs.contains("en"), "{country}: no English release");
        assert!(
            langs.contains(own_language(country)),
            "{country}: no release in {}",
            own_language(country)
        );
        let sectors = &coverage
            .sectors
            .iter()
            .find(|(c, _)| c == country)
            .expect("the country has fixtures")
            .1;
        for sector in SECTORS {
            assert!(sectors.contains(sector), "{country}: no {sector} fixture");
        }
    }
}

#[test]
fn every_fixture_is_consistent() {
    for fixture in fixtures() {
        let name = &fixture.name;
        let expected = &fixture.expected;
        assert!(
            name.starts_with(&format!("{}-", expected.country.to_lowercase())),
            "{name}: the directory does not start with its country"
        );
        assert!(
            SECTORS.contains(&expected.sector.as_str()),
            "{name}: unknown sector {}",
            expected.sector
        );
        assert!(
            ["interim", "annual"].contains(&expected.release.kind.as_str()),
            "{name}: unknown kind {}",
            expected.release.kind
        );
        assert!(
            fixture
                .text
                .starts_with(&format!("{}\n\n", expected.release.title)),
            "{name}: release.txt does not start with the title and a blank line"
        );
        assert!(
            expected.previous_reports.len() >= 4,
            "{name}: only {} previous reports",
            expected.previous_reports.len()
        );
        for report in &expected.previous_reports {
            assert!(
                report.published_at < expected.release.published_at,
                "{name}: previous report {} is not earlier",
                report.news_id
            );
            assert_ne!(report.news_id, expected.release.news_id);
        }
        let count = expected.metrics.len();
        assert!(
            (4..=6).contains(&count),
            "{name}: {count} metrics, need 4 to 6"
        );
        let keys: BTreeSet<&str> = expected.metrics.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys.len(), count, "{name}: a metric key is repeated");
    }
}

#[test]
fn every_hand_checked_figure_is_printed_where_its_fixture_says() {
    for fixture in fixtures() {
        for metric in &fixture.expected.metrics {
            let at = format!("{} {}", fixture.name, metric.key);
            assert!(
                metric
                    .key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{at}: the key is not snake_case"
            );
            for (field, value, line) in [
                ("current", &metric.current, &metric.current_line),
                ("prior", &metric.prior, &metric.prior_line),
            ] {
                assert!(
                    Decimal::from_str(value).is_ok() && !value.contains(['e', 'E']),
                    "{at}: {field} {value:?} is not an exact decimal string"
                );
                assert!(
                    !line.is_empty() && fixture.text.contains(line.as_str()),
                    "{at}: the {field} line is not in release.txt: {line:?}"
                );
                assert!(
                    line_shows(line, value),
                    "{at}: {field} {value} is not printed in {line:?}"
                );
            }
        }
    }
}

#[test]
fn no_dry_run_input_contains_its_release() {
    for fixture in fixtures() {
        let input = serde_json::to_string(&fixture.dry_run_input()).expect("serializable");
        assert!(
            !input.contains(&fixture.expected.release.news_id),
            "{}: the dry-run input names the release itself",
            fixture.name
        );
        assert_eq!(
            fixture.dry_run_input().dry_run.metrics.len(),
            fixture.expected.metrics.len()
        );
    }
}

fn answering_kernel(json: &str) -> Vec<u8> {
    let escaped = json.replace('\\', "\\\\").replace('"', "\\\"");
    wat::parse_str(format!(
        r#"(module
  (memory (export "memory") 17)
  (data (i32.const 1024) "{escaped}")
  (func (export "verdict_input") (param i32) (result i32) (i32.const 65536))
  (func (export "verdict_run") (param i32 i32) (result i64)
    (i64.or (i64.shl (i64.const 1024) (i64.const 32)) (i64.const {len}))))"#,
        len = json.len()
    ))
    .expect("valid WAT")
}

fn run(fixture: &Fixture, json: &str) -> KernelResult {
    let runtime = KernelRuntime::new(RuntimeConfig::default()).expect("the engine starts");
    let kernel = runtime
        .load(1, &answering_kernel(json))
        .expect("the kernel loads");
    match kernel.run(fixture.text.as_bytes(), fixture.lang()) {
        RunOutcome::Ok { verdict, .. } => KernelResult::Verdict(verdict),
        RunOutcome::Err(error) => KernelResult::Failed(error.to_string()),
    }
}

#[test]
fn a_kernel_reporting_the_hand_checked_figures_scores_all_exact_through_the_runtime() {
    let fixture = fixtures().remove(0);
    let metrics: Vec<serde_json::Value> = fixture
        .expected
        .metrics
        .iter()
        .map(|m| serde_json::json!({"key": m.key, "current": m.current, "prior": m.prior}))
        .collect();
    let json = serde_json::json!({"decision": "abstain", "reason": "dry run", "metrics": metrics})
        .to_string();
    let scores = score(&fixture, &run(&fixture, &json));
    let summary = Summary::of(&scores);
    assert_eq!(summary.exact, fixture.expected.metrics.len() * 2);
    assert!(summary.passes());
}

#[test]
fn one_wrong_figure_fails_the_run_however_many_are_exact() {
    let fixture = fixtures().remove(0);
    let mut metrics: Vec<serde_json::Value> = fixture
        .expected
        .metrics
        .iter()
        .map(|m| serde_json::json!({"key": m.key, "current": m.current, "prior": m.prior}))
        .collect();
    metrics[0]["prior"] = serde_json::json!("999999.9");
    let json =
        serde_json::json!({"decision": "buy", "reason": "x", "metrics": metrics}).to_string();
    let scores = score(&fixture, &run(&fixture, &json));
    let summary = Summary::of(&scores);
    assert_eq!(summary.wrong, 1);
    assert!(!summary.passes());
    assert!(scores.iter().any(|s| s.grade
        == Grade::Wrong {
            got: "999999.9".to_owned()
        }));
}
