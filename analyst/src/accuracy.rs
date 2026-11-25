//! Extraction accuracy: the hand-checked fixtures under `analyst/tests/accuracy/`,
//! the dry-run preparation input built from each, and the score of a kernel's
//! figures against the hand-checked ones.
//!
//! Only extraction is scored, never a decision: a kernel prepared today for a
//! past report can have seen the outcome. A figure is `exact` when it equals the
//! hand-checked decimal, `missing` when the kernel reported none (acceptable),
//! and `wrong` when it reported a different one (never acceptable). Nothing here
//! touches a database, Kafka or a quote, so scoring can never create a trade.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::verdict::KernelVerdict;

/// The share of hand-checked values, in percent, that must be exactly right.
pub const PASS_EXACT_PERCENT: u32 = 95;

/// How long before the release the dry run pretends the preparation started
/// (the default `PREP_LEAD`).
pub const DRY_RUN_LEAD: Duration = Duration::hours(12);

/// A fixture could not be read.
#[derive(Debug, Error)]
pub enum FixtureError {
    /// A file could not be read.
    #[error("cannot read {path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// `expected.json` is not valid.
    #[error("{path} is not a valid expected.json: {source}")]
    Json {
        /// The file.
        path: PathBuf,
        /// Why.
        source: serde_json::Error,
    },
}

/// The company of a fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureCompany {
    /// The ticker, as on the exchange.
    pub ticker: String,
    /// The exchange's MIC.
    pub exchange_mic: String,
    /// The company's name on MFN.
    pub name: String,
    /// The company's MFN slug.
    pub mfn_slug: String,
}

/// The release a fixture holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureRelease {
    /// MFN's id of the release.
    pub news_id: String,
    /// The release's language tag.
    pub lang: String,
    /// The release's title.
    pub title: String,
    /// When MFN published it.
    pub published_at: DateTime<Utc>,
    /// The release on MFN.
    pub url: String,
    /// `interim` or `annual`.
    pub kind: String,
}

/// One earlier report release of the company: the input of a dry run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviousReport {
    /// MFN's id of the release.
    pub news_id: String,
    /// The release's language tag.
    pub lang: String,
    /// The release's title.
    pub title: String,
    /// When MFN published it.
    pub published_at: DateTime<Utc>,
    /// The release on MFN.
    pub url: String,
}

/// One hand-checked metric.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedMetric {
    /// The metric's key, which the dry-run kernel must use.
    pub key: String,
    /// Its human name.
    pub label: String,
    /// The unit as printed.
    pub unit: String,
    /// This period's figure, an exact decimal string.
    pub current: String,
    /// The comparison period's figure, an exact decimal string.
    pub prior: String,
    /// An exact substring of the release that shows `current`.
    pub current_line: String,
    /// An exact substring of the release that shows `prior`.
    pub prior_line: String,
}

/// A fixture's `expected.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expected {
    /// `SE`, `DK` or `FI`.
    pub country: String,
    /// `industrial`, `bank`, `property` or `biotech`.
    pub sector: String,
    /// The company.
    pub company: FixtureCompany,
    /// The release.
    pub release: FixtureRelease,
    /// The company's latest report releases before this one, newest first.
    pub previous_reports: Vec<PreviousReport>,
    /// The hand-checked metrics.
    pub metrics: Vec<ExpectedMetric>,
}

/// One fixture: its name (the directory), the release text and what is expected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixture {
    /// The fixture's directory name.
    pub name: String,
    /// `release.txt`: exactly what the runtime sees.
    pub text: String,
    /// `expected.json`.
    pub expected: Expected,
}

impl Fixture {
    /// Reads the fixture in `dir`.
    pub fn load(dir: &Path) -> Result<Fixture, FixtureError> {
        let read = |file: &str| {
            let path = dir.join(file);
            std::fs::read_to_string(&path).map_err(|source| FixtureError::Io { path, source })
        };
        let text = read("release.txt")?;
        let raw = read("expected.json")?;
        let expected = serde_json::from_str(&raw).map_err(|source| FixtureError::Json {
            path: dir.join("expected.json"),
            source,
        })?;
        let name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(Fixture {
            name,
            text,
            expected,
        })
    }

    /// The release's language, as the runtime passes it to a kernel.
    pub fn lang(&self) -> verdict_sdk::Lang {
        verdict_sdk::Lang::from_tag(&self.expected.release.lang)
    }

    /// The `PREP_INPUT` of the dry preparation for this fixture: the fixture's
    /// report as the next report, only the earlier reports as input, and the
    /// keys the kernel must report. The release itself is not in it.
    pub fn dry_run_input(&self) -> DryRunInput {
        let expected = &self.expected;
        let mut languages: Vec<String> = Vec::new();
        for lang in std::iter::once(&expected.release.lang)
            .chain(expected.previous_reports.iter().map(|report| &report.lang))
        {
            if !languages.contains(lang) {
                languages.push(lang.clone());
            }
        }
        DryRunInput {
            company: DryRunCompany {
                ticker: expected.company.ticker.clone(),
                exchange_mic: expected.company.exchange_mic.clone(),
                name: expected.company.name.clone(),
                mfn_slug: expected.company.mfn_slug.clone(),
                currency: currency_of(&expected.country).to_owned(),
                news_url: format!("https://mfn.se/all/a/{}", expected.company.mfn_slug),
            },
            report: DryRunReport {
                kind: expected.release.kind.clone(),
                fiscal_period: None,
                expected_at: expected.release.published_at,
                expected_time_known: true,
            },
            languages,
            previous_reports: expected.previous_reports.clone(),
            notes: Vec::new(),
            prep_started_at: expected.release.published_at - DRY_RUN_LEAD,
            dry_run: DryRunSpec {
                fixture: self.name.clone(),
                metrics: expected
                    .metrics
                    .iter()
                    .map(|metric| DryRunMetric {
                        key: metric.key.clone(),
                        label: metric.label.clone(),
                        unit: metric.unit.clone(),
                    })
                    .collect(),
            },
        }
    }
}

fn currency_of(country: &str) -> &'static str {
    match country {
        "SE" => "SEK",
        "DK" => "DKK",
        "FI" => "EUR",
        "NO" => "NOK",
        _ => "",
    }
}

/// Reads every fixture directory under `root` (each holding an
/// `expected.json`), sorted by name.
pub fn load_all(root: &Path) -> Result<Vec<Fixture>, FixtureError> {
    let entries = std::fs::read_dir(root).map_err(|source| FixtureError::Io {
        path: root.to_owned(),
        source,
    })?;
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| FixtureError::Io {
            path: root.to_owned(),
            source,
        })?;
        let path = entry.path();
        if path.join("expected.json").is_file() {
            dirs.push(path);
        }
    }
    dirs.sort();
    dirs.iter().map(|dir| Fixture::load(dir)).collect()
}

/// The company in a dry run's `PREP_INPUT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunCompany {
    /// The ticker.
    pub ticker: String,
    /// The exchange's MIC.
    pub exchange_mic: String,
    /// The company's name.
    pub name: String,
    /// The company's MFN slug.
    pub mfn_slug: String,
    /// The reporting currency of the country.
    pub currency: String,
    /// The company's MFN news page.
    pub news_url: String,
}

/// The report in a dry run's `PREP_INPUT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunReport {
    /// `interim` or `annual`.
    pub kind: String,
    /// The fiscal period, when known.
    pub fiscal_period: Option<String>,
    /// When the report is due: the fixture's publish instant.
    pub expected_at: DateTime<Utc>,
    /// Whether the time of day is known.
    pub expected_time_known: bool,
}

/// One metric a dry-run kernel must report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunMetric {
    /// The key the kernel must use.
    pub key: String,
    /// Its human name.
    pub label: String,
    /// The unit as the release prints it.
    pub unit: String,
}

/// The dry-run part of `PREP_INPUT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunSpec {
    /// The fixture this run is for.
    pub fixture: String,
    /// The metrics the kernel must report.
    pub metrics: Vec<DryRunMetric>,
}

/// A dry run's `PREP_INPUT` (`verdict/prep/README.md`), with `dry_run` added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunInput {
    /// The company.
    pub company: DryRunCompany,
    /// The report to prepare for.
    pub report: DryRunReport,
    /// The languages the company publishes in.
    pub languages: Vec<String>,
    /// The earlier report releases.
    pub previous_reports: Vec<PreviousReport>,
    /// No notes in a dry run.
    pub notes: Vec<serde_json::Value>,
    /// When the preparation pretends to start.
    pub prep_started_at: DateTime<Utc>,
    /// The metrics to report.
    pub dry_run: DryRunSpec,
}

/// Which figure of a metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    /// This period's figure.
    Current,
    /// The comparison period's figure.
    Prior,
}

impl Field {
    /// `current` or `prior`.
    pub fn as_str(self) -> &'static str {
        match self {
            Field::Current => "current",
            Field::Prior => "prior",
        }
    }
}

/// How one hand-checked value was read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "grade", rename_all = "snake_case")]
pub enum Grade {
    /// The kernel's figure equals the hand-checked one.
    Exact,
    /// The kernel reported no figure.
    Missing {
        /// Why there is none.
        cause: String,
    },
    /// The kernel reported a different figure.
    Wrong {
        /// What it reported.
        got: String,
    },
}

/// The score of one hand-checked value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValueScore {
    /// The fixture.
    pub fixture: String,
    /// The metric's key.
    pub key: String,
    /// Which figure.
    pub field: Field,
    /// The hand-checked figure.
    pub expected: String,
    /// How it was read.
    #[serde(flatten)]
    pub grade: Grade,
}

/// What running a fixture's kernel produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelResult {
    /// The kernel returned a verdict.
    Verdict(KernelVerdict),
    /// There is no kernel, or it failed to load or run.
    Failed(String),
}

fn same_decimal(got: &str, expected: &str) -> bool {
    match (Decimal::from_str(got.trim()), Decimal::from_str(expected)) {
        (Ok(got), Ok(expected)) => got == expected,
        _ => false,
    }
}

/// Scores every hand-checked value of `fixture` against what its kernel returned.
pub fn score(fixture: &Fixture, result: &KernelResult) -> Vec<ValueScore> {
    let mut scores = Vec::with_capacity(fixture.expected.metrics.len() * 2);
    for metric in &fixture.expected.metrics {
        for (field, expected) in [
            (Field::Current, &metric.current),
            (Field::Prior, &metric.prior),
        ] {
            let grade = match result {
                KernelResult::Failed(cause) => Grade::Missing {
                    cause: cause.clone(),
                },
                KernelResult::Verdict(verdict) => {
                    let reported: Vec<_> = verdict
                        .metrics
                        .iter()
                        .filter(|found| found.key == metric.key)
                        .collect();
                    let values: Vec<&str> = reported
                        .iter()
                        .filter_map(|found| match field {
                            Field::Current => found.current.as_deref(),
                            Field::Prior => found.prior.as_deref(),
                        })
                        .collect();
                    if reported.is_empty() {
                        Grade::Missing {
                            cause: "metric not reported".to_owned(),
                        }
                    } else if values.is_empty() {
                        Grade::Missing {
                            cause: format!("{} is null", field.as_str()),
                        }
                    } else if let Some(wrong) =
                        values.iter().find(|value| !same_decimal(value, expected))
                    {
                        Grade::Wrong {
                            got: (*wrong).to_owned(),
                        }
                    } else {
                        Grade::Exact
                    }
                }
            };
            scores.push(ValueScore {
                fixture: fixture.name.clone(),
                key: metric.key.clone(),
                field,
                expected: expected.clone(),
                grade,
            });
        }
    }
    scores
}

/// The totals over every scored value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// Hand-checked values.
    pub total: usize,
    /// Read exactly.
    pub exact: usize,
    /// Not reported.
    pub missing: usize,
    /// Reported wrong.
    pub wrong: usize,
}

impl Summary {
    /// Counts the grades of `scores`.
    pub fn of(scores: &[ValueScore]) -> Summary {
        let mut summary = Summary {
            total: scores.len(),
            ..Summary::default()
        };
        for score in scores {
            match score.grade {
                Grade::Exact => summary.exact += 1,
                Grade::Missing { .. } => summary.missing += 1,
                Grade::Wrong { .. } => summary.wrong += 1,
            }
        }
        summary
    }

    /// The share read exactly, in percent.
    pub fn exact_percent(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.exact as f64 * 100.0 / self.total as f64
    }

    /// At least [`PASS_EXACT_PERCENT`] exact and nothing wrong.
    pub fn passes(&self) -> bool {
        self.total > 0
            && self.wrong == 0
            && self.exact * 100 >= self.total * PASS_EXACT_PERCENT as usize
    }
}

/// The markdown result of a scoring run: the totals, one row per fixture and
/// every miss, for `docs/validation/analyst-accuracy.md`.
pub fn render_markdown(fixtures: &[Fixture], scores: &[ValueScore]) -> String {
    let summary = Summary::of(scores);
    let mut out = String::new();
    let verdict = if summary.passes() { "PASS" } else { "FAIL" };
    let _ = writeln!(
        out,
        "**{verdict}**: {exact} of {total} values exact ({percent:.1} %, target ≥ {PASS_EXACT_PERCENT} %), {missing} missing, {wrong} wrong (target 0).\n",
        exact = summary.exact,
        total = summary.total,
        percent = summary.exact_percent(),
        missing = summary.missing,
        wrong = summary.wrong,
    );
    out.push_str("| Fixture | Company | Country | Sector | Lang | Exact | Missing | Wrong |\n");
    out.push_str("|---|---|---|---|---|---|---|---|\n");
    for fixture in fixtures {
        let own: Vec<ValueScore> = scores
            .iter()
            .filter(|score| score.fixture == fixture.name)
            .cloned()
            .collect();
        let counts = Summary::of(&own);
        let expected = &fixture.expected;
        let _ = writeln!(
            out,
            "| `{}` | {} ({}:{}) | {} | {} | {} | {}/{} | {} | {} |",
            fixture.name,
            expected.company.name,
            expected.company.exchange_mic,
            expected.company.ticker,
            expected.country,
            expected.sector,
            expected.release.lang,
            counts.exact,
            counts.total,
            counts.missing,
            counts.wrong,
        );
    }
    let misses: Vec<&ValueScore> = scores
        .iter()
        .filter(|score| score.grade != Grade::Exact)
        .collect();
    out.push_str("\n### Misses\n\n");
    if misses.is_empty() {
        out.push_str("None.\n");
        return out;
    }
    out.push_str("| Fixture | Metric | Figure | Expected | Grade | Cause |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    for miss in misses {
        let (grade, cause) = match &miss.grade {
            Grade::Exact => continue,
            Grade::Missing { cause } => ("missing", cause.clone()),
            Grade::Wrong { got } => ("**wrong**", format!("read `{got}`")),
        };
        let _ = writeln!(
            out,
            "| `{}` | `{}` | {} | `{}` | {} | {} |",
            miss.fixture,
            miss.key,
            miss.field.as_str(),
            miss.expected,
            grade,
            cause.replace('|', "\\|"),
        );
    }
    out
}

/// What the fixture set covers, for the coverage rules in the fixtures' README.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    /// `(country, fixtures)`.
    pub per_country: Vec<(String, usize)>,
    /// `(country, languages)`.
    pub languages: Vec<(String, BTreeSet<String>)>,
    /// `(country, sectors)`.
    pub sectors: Vec<(String, BTreeSet<String>)>,
}

/// Counts what `fixtures` cover per country.
pub fn coverage(fixtures: &[Fixture]) -> Coverage {
    let countries: BTreeSet<String> = fixtures
        .iter()
        .map(|fixture| fixture.expected.country.clone())
        .collect();
    fn of<'a>(fixtures: &'a [Fixture], country: &'a str) -> impl Iterator<Item = &'a Fixture> + 'a {
        fixtures
            .iter()
            .filter(move |fixture| fixture.expected.country == country)
    }
    Coverage {
        per_country: countries
            .iter()
            .map(|country| (country.clone(), of(fixtures, country).count()))
            .collect(),
        languages: countries
            .iter()
            .map(|country| {
                let langs = of(fixtures, country)
                    .map(|f| f.expected.release.lang.clone())
                    .collect();
                (country.clone(), langs)
            })
            .collect(),
        sectors: countries
            .iter()
            .map(|country| {
                let sectors = of(fixtures, country)
                    .map(|f| f.expected.sector.clone())
                    .collect();
                (country.clone(), sectors)
            })
            .collect(),
    }
}

/// The figure's digits as they can appear in a release: the absolute value
/// with no sign, so `-12.30` checks `12.30`, `12,30`, `12.3` and `12,3`.
pub fn printed_forms(value: &str) -> Vec<String> {
    let digits = value.trim().trim_start_matches(['-', '+']);
    let mut forms = vec![digits.to_owned()];
    if digits.contains('.') {
        let trimmed = digits.trim_end_matches('0').trim_end_matches('.');
        if trimmed != digits {
            forms.push(trimmed.to_owned());
        }
    }
    let commas: Vec<String> = forms.iter().map(|form| form.replace('.', ",")).collect();
    forms.extend(commas);
    forms
}

/// The line with every thousands separator a Nordic or English release uses
/// removed, in both readings: `.` as the thousands separator (Danish), and `,`
/// as the thousands separator (English). Spaces are always removed. Each figure
/// of [`printed_forms`] should be in one of them.
pub fn normalised_lines(line: &str) -> [String; 2] {
    let chars: Vec<char> = line
        .chars()
        .filter(|c| !matches!(c, ' ' | '\u{a0}' | '\u{202f}' | '\u{2009}' | '\''))
        .collect();
    [
        without_thousands(&chars, '.'),
        without_thousands(&chars, ','),
    ]
}

fn without_thousands(chars: &[char], separator: char) -> String {
    let digit = |i: usize| chars.get(i).is_some_and(char::is_ascii_digit);
    chars
        .iter()
        .enumerate()
        .filter(|&(i, &c)| {
            let thousands = c == separator
                && i > 0
                && digit(i - 1)
                && digit(i + 1)
                && digit(i + 2)
                && digit(i + 3)
                && !digit(i + 4);
            !thousands
        })
        .map(|(_, &c)| c)
        .collect()
}

/// Whether the figure `value` is printed in `line`, in any of the forms a
/// release uses.
pub fn line_shows(line: &str, value: &str) -> bool {
    let lines = normalised_lines(line);
    let spaceless: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    printed_forms(value).iter().any(|form| {
        lines.iter().any(|line| line.contains(form.as_str())) || spaceless.contains(form.as_str())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verdict::KernelMetric;
    use verdict_sdk::Decision;

    fn metric(key: &str, current: &str, prior: &str) -> ExpectedMetric {
        ExpectedMetric {
            key: key.to_owned(),
            label: key.to_owned(),
            unit: "MSEK".to_owned(),
            current: current.to_owned(),
            prior: prior.to_owned(),
            current_line: String::new(),
            prior_line: String::new(),
        }
    }

    fn fixture(metrics: Vec<ExpectedMetric>) -> Fixture {
        let at = |s: &str| {
            DateTime::parse_from_rfc3339(s)
                .expect("valid instant")
                .to_utc()
        };
        Fixture {
            name: "se-acme-2026q3".to_owned(),
            text: "Acme Q3\n\nNet sales 1 234,5 (1 101,0)".to_owned(),
            expected: Expected {
                country: "SE".to_owned(),
                sector: "industrial".to_owned(),
                company: FixtureCompany {
                    ticker: "ACME B".to_owned(),
                    exchange_mic: "XSTO".to_owned(),
                    name: "Acme".to_owned(),
                    mfn_slug: "acme".to_owned(),
                },
                release: FixtureRelease {
                    news_id: "target".to_owned(),
                    lang: "sv".to_owned(),
                    title: "Acme Q3".to_owned(),
                    published_at: at("2026-10-23T05:30:00Z"),
                    url: "https://mfn.se/a/acme/q3".to_owned(),
                    kind: "interim".to_owned(),
                },
                previous_reports: vec![
                    PreviousReport {
                        news_id: "q2-en".to_owned(),
                        lang: "en".to_owned(),
                        title: "Acme Q2".to_owned(),
                        published_at: at("2026-07-17T05:30:00Z"),
                        url: "https://mfn.se/a/acme/q2-en".to_owned(),
                    },
                    PreviousReport {
                        news_id: "q2-sv".to_owned(),
                        lang: "sv".to_owned(),
                        title: "Acme Q2".to_owned(),
                        published_at: at("2026-07-17T05:30:00Z"),
                        url: "https://mfn.se/a/acme/q2-sv".to_owned(),
                    },
                ],
                metrics,
            },
        }
    }

    fn found(key: &str, current: Option<&str>, prior: Option<&str>) -> KernelMetric {
        KernelMetric {
            key: key.to_owned(),
            current: current.map(str::to_owned),
            prior: prior.map(str::to_owned),
            expected: None,
            unit: None,
        }
    }

    fn verdict(metrics: Vec<KernelMetric>) -> KernelResult {
        KernelResult::Verdict(KernelVerdict {
            decision: Decision::Abstain,
            reason: "dry run".to_owned(),
            metrics,
        })
    }

    fn grades(scores: &[ValueScore]) -> Vec<(&str, Field, &Grade)> {
        scores
            .iter()
            .map(|score| (score.key.as_str(), score.field, &score.grade))
            .collect()
    }

    #[test]
    fn equal_decimals_are_exact_whatever_trailing_zeros() {
        let fixture = fixture(vec![metric("net_sales", "1234.5", "-12.30")]);
        let scores = score(
            &fixture,
            &verdict(vec![found("net_sales", Some("1234.50"), Some("-12.3"))]),
        );
        assert_eq!(
            grades(&scores),
            vec![
                ("net_sales", Field::Current, &Grade::Exact),
                ("net_sales", Field::Prior, &Grade::Exact)
            ]
        );
    }

    #[test]
    fn a_different_figure_or_sign_is_wrong() {
        let fixture = fixture(vec![metric("ebit", "12.3", "-4")]);
        let scores = score(
            &fixture,
            &verdict(vec![found("ebit", Some("123"), Some("4"))]),
        );
        assert_eq!(
            grades(&scores),
            vec![
                (
                    "ebit",
                    Field::Current,
                    &Grade::Wrong {
                        got: "123".to_owned()
                    }
                ),
                (
                    "ebit",
                    Field::Prior,
                    &Grade::Wrong {
                        got: "4".to_owned()
                    }
                )
            ]
        );
    }

    #[test]
    fn an_unreadable_figure_is_wrong_not_missing() {
        let fixture = fixture(vec![metric("ebit", "12.3", "4")]);
        let scores = score(
            &fixture,
            &verdict(vec![found("ebit", Some("12,3"), Some("4"))]),
        );
        assert_eq!(
            scores[0].grade,
            Grade::Wrong {
                got: "12,3".to_owned()
            }
        );
        assert_eq!(scores[1].grade, Grade::Exact);
    }

    #[test]
    fn an_absent_metric_or_null_figure_is_missing() {
        let fixture = fixture(vec![metric("ebit", "1", "2"), metric("eps", "3", "4")]);
        let scores = score(&fixture, &verdict(vec![found("ebit", Some("1"), None)]));
        assert_eq!(
            grades(&scores),
            vec![
                ("ebit", Field::Current, &Grade::Exact),
                (
                    "ebit",
                    Field::Prior,
                    &Grade::Missing {
                        cause: "prior is null".to_owned()
                    }
                ),
                (
                    "eps",
                    Field::Current,
                    &Grade::Missing {
                        cause: "metric not reported".to_owned()
                    }
                ),
                (
                    "eps",
                    Field::Prior,
                    &Grade::Missing {
                        cause: "metric not reported".to_owned()
                    }
                ),
            ]
        );
    }

    #[test]
    fn a_key_reported_twice_with_one_wrong_figure_is_wrong() {
        let fixture = fixture(vec![metric("ebit", "1", "2")]);
        let scores = score(
            &fixture,
            &verdict(vec![
                found("ebit", Some("1"), Some("2")),
                found("ebit", Some("7"), Some("2")),
            ]),
        );
        assert_eq!(
            scores[0].grade,
            Grade::Wrong {
                got: "7".to_owned()
            }
        );
        assert_eq!(scores[1].grade, Grade::Exact);
    }

    #[test]
    fn a_failed_kernel_misses_every_value() {
        let fixture = fixture(vec![metric("ebit", "1", "2")]);
        let scores = score(&fixture, &KernelResult::Failed("no kernel".to_owned()));
        assert!(scores.iter().all(|score| score.grade
            == Grade::Missing {
                cause: "no kernel".to_owned()
            }));
    }

    #[test]
    fn the_pass_rule_needs_95_percent_and_nothing_wrong() {
        let pass = Summary {
            total: 100,
            exact: 95,
            missing: 5,
            wrong: 0,
        };
        assert!(pass.passes());
        assert!(!Summary {
            exact: 94,
            missing: 6,
            ..pass
        }
        .passes());
        assert!(!Summary {
            exact: 99,
            missing: 0,
            wrong: 1,
            ..pass
        }
        .passes());
        assert!(!Summary::default().passes());
    }

    #[test]
    fn the_dry_run_input_holds_the_earlier_reports_and_the_keys_but_not_the_release() {
        let fixture = fixture(vec![metric("net_sales", "1234.5", "1101.0")]);
        let input = fixture.dry_run_input();
        let json = serde_json::to_value(&input).expect("serializable");
        assert_eq!(json["report"]["expected_at"], "2026-10-23T05:30:00Z");
        assert_eq!(json["prep_started_at"], "2026-10-22T17:30:00Z");
        assert_eq!(json["company"]["currency"], "SEK");
        assert_eq!(json["company"]["news_url"], "https://mfn.se/all/a/acme");
        assert_eq!(json["languages"], serde_json::json!(["sv", "en"]));
        assert_eq!(
            json["dry_run"]["metrics"],
            serde_json::json!([{"key": "net_sales", "label": "net_sales", "unit": "MSEK"}])
        );
        let ids: Vec<&str> = input
            .previous_reports
            .iter()
            .map(|report| report.news_id.as_str())
            .collect();
        assert_eq!(ids, vec!["q2-en", "q2-sv"]);
        assert!(!json.to_string().contains("\"target\""));
        assert!(!json.to_string().contains("1234.5"));
    }

    #[test]
    fn the_markdown_lists_every_miss_and_the_verdict() {
        let fixture = fixture(vec![metric("ebit", "1", "2")]);
        let scores = score(
            &fixture,
            &verdict(vec![found("ebit", Some("1"), Some("3"))]),
        );
        let markdown = render_markdown(std::slice::from_ref(&fixture), &scores);
        assert!(markdown.starts_with("**FAIL**: 1 of 2 values exact (50.0 %"));
        assert!(markdown.contains(
            "| `se-acme-2026q3` | Acme (XSTO:ACME B) | SE | industrial | sv | 1/2 | 0 | 1 |"
        ));
        assert!(
            markdown.contains("| `se-acme-2026q3` | `ebit` | prior | `2` | **wrong** | read `3` |")
        );
    }

    #[test]
    fn printed_figures_are_found_in_every_nordic_and_english_format() {
        assert!(line_shows(
            "Net sales SEK 1,234.5 million (1,101.0)",
            "1234.5"
        ));
        assert!(line_shows(
            "Nettoomsättning 1 234,5 MSEK (1 101,0)",
            "1101.0"
        ));
        assert!(line_shows("Omsætning 1.234,5 mio. kr. (1.101,0)", "1234.5"));
        assert!(line_shows("Liikevaihto 1\u{a0}234,5 milj. euroa", "1234.5"));
        assert!(line_shows("EPS −0,12 euroa (0,30)", "-0.12"));
        assert!(line_shows("result (12.3)", "-12.3"));
        assert!(line_shows("margin 12.30%", "12.3"));
        assert!(line_shows("CET1 18.4 per cent", "18.4"));
        assert!(!line_shows("Net sales SEK 1,234.5 million", "1243.5"));
        assert!(!line_shows("Net sales SEK 12.0 billion", "13"));
    }
}
