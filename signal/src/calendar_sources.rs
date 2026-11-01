//! Which of a company's scheduled rows win when the MFN page and a calendar
//! release both list the same report (NEX-225).
//!
//! Pure: rows in, links out, no SQL and no clock beyond `today`. The newest
//! source wins; the loser keeps its row and points at the winner through
//! `superseded_by`, so the result depends only on the stored rows.

use crate::reports::MATCH_WINDOW_DAYS;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use std::collections::HashMap;

/// `special_events.source` of rows that come from the MFN page.
pub const SOURCE_PAGE: &str = "mfn";
/// `special_events.source` of rows that come from a calendar release.
pub const SOURCE_RELEASE: &str = "mfn_release";

/// A scheduled row, as far as resolution needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedRow {
    /// `special_events.id`.
    pub id: i64,
    /// [`SOURCE_PAGE`] or [`SOURCE_RELEASE`].
    pub source: String,
    /// `interim` or `annual`.
    pub report_kind: String,
    /// `2026-Q3`, `2026`, or unknown.
    pub fiscal_period: Option<String>,
    /// The date the row expects the report on.
    pub expected_date: NaiveDate,
    /// When the source stated it: the snapshot's `fetched_at` for the page, the
    /// publish instant for a release.
    pub fetched_at: Option<DateTime<Utc>>,
}

impl SchedRow {
    fn is_release(&self) -> bool {
        self.source == SOURCE_RELEASE
    }

    fn is_page(&self) -> bool {
        self.source == SOURCE_PAGE
    }

    /// `<kind>:<fiscal_period>`, or `<kind>:<date>` when the period is unknown.
    pub fn report_key(&self) -> String {
        match &self.fiscal_period {
            Some(p) => format!("{}:{p}", self.report_kind),
            None => format!("{}:{}", self.report_kind, self.expected_date),
        }
    }
}

/// Whether two rows are the same report: the same kind and, when both periods
/// are known, the same period; otherwise the same kind within
/// [`MATCH_WINDOW_DAYS`].
pub fn same_report(a: &SchedRow, b: &SchedRow) -> bool {
    if a.report_kind != b.report_kind {
        return false;
    }
    match (&a.fiscal_period, &b.fiscal_period) {
        (Some(x), Some(y)) => x == y,
        _ => {
            (a.expected_date - b.expected_date).abs()
                <= Duration::days(i64::from(MATCH_WINDOW_DAYS))
        }
    }
}

/// A page row and a release row that disagree on the date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// [`SchedRow::report_key`] of the page row.
    pub report_key: String,
    /// The page's date.
    pub page_date: NaiveDate,
    /// The release's date.
    pub release_date: NaiveDate,
    /// `page` or `release`.
    pub winner: &'static str,
}

/// What resolution decided.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution {
    /// `(loser_id, winner_id)`, ordered by loser.
    pub links: Vec<(i64, i64)>,
    /// `(page_row_id, fiscal_period)`: periods a release supplies to a page row
    /// that has none.
    pub period_fills: Vec<(i64, String)>,
    /// Disagreements, for logging.
    pub conflicts: Vec<Conflict>,
}

/// [`resolve`], with the period fills and conflicts too.
pub fn resolve_detailed(rows: &[SchedRow], today: NaiveDate) -> Resolution {
    let live: Vec<&SchedRow> = rows
        .iter()
        .filter(|r| r.expected_date >= today && (r.is_page() || r.is_release()))
        .collect();
    let mut out = Resolution::default();
    let mut links: HashMap<i64, (i64, Option<DateTime<Utc>>)> = HashMap::new();

    for rel in live.iter().copied().filter(|r| r.is_release()) {
        let page = live
            .iter()
            .copied()
            .filter(|p| p.is_page() && same_report(p, rel))
            .min_by_key(|p| {
                (
                    p.fiscal_period.is_none() || rel.fiscal_period.is_none(),
                    (p.expected_date - rel.expected_date).num_days().abs(),
                    p.id,
                )
            });
        let Some(page) = page else { continue };
        let (loser, winner) = if page.expected_date == rel.expected_date {
            if page.fiscal_period.is_none() {
                if let Some(p) = &rel.fiscal_period {
                    out.period_fills.push((page.id, p.clone()));
                }
            }
            (rel, page)
        } else {
            let release_wins = rel.fetched_at > page.fetched_at;
            out.conflicts.push(Conflict {
                report_key: page.report_key(),
                page_date: page.expected_date,
                release_date: rel.expected_date,
                winner: if release_wins { "release" } else { "page" },
            });
            if release_wins {
                (page, rel)
            } else {
                (rel, page)
            }
        };
        let better = links
            .get(&loser.id)
            .is_none_or(|(_, at)| winner.fetched_at > *at);
        if better {
            links.insert(loser.id, (winner.id, winner.fetched_at));
        }
    }
    let mut pairs: Vec<(i64, i64)> = links.into_iter().map(|(l, (w, _))| (l, w)).collect();
    pairs.sort_unstable();
    out.links = pairs;
    out
}

#[cfg(test)]
/// `(loser_id, winner_id)` for every scheduled row that another source's row
/// outranks. Rows before `today` are not compared.
pub fn resolve(rows: &[SchedRow], today: NaiveDate) -> Vec<(i64, i64)> {
    resolve_detailed(rows, today).links
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn row(id: i64, source: &str, period: Option<&str>, date: &str, at: &str) -> SchedRow {
        SchedRow {
            id,
            source: source.into(),
            report_kind: "interim".into(),
            fiscal_period: period.map(String::from),
            expected_date: d(date),
            fetched_at: Some(at.parse().unwrap()),
        }
    }

    const TODAY: &str = "2026-11-01";

    #[test]
    fn a_release_row_alone_wins() {
        let rows = [row(
            1,
            SOURCE_RELEASE,
            Some("2026-Q3"),
            "2026-11-24",
            "2025-12-23T09:38:59Z",
        )];
        assert!(resolve(&rows, d(TODAY)).is_empty());
    }

    #[test]
    fn same_report_same_date_the_page_wins_and_the_release_fills_the_period() {
        let rows = [
            row(1, SOURCE_PAGE, None, "2026-11-24", "2026-10-01T00:00:00Z"),
            row(
                2,
                SOURCE_RELEASE,
                Some("2026-Q3"),
                "2026-11-24",
                "2025-12-23T00:00:00Z",
            ),
        ];
        let r = resolve_detailed(&rows, d(TODAY));
        assert_eq!(r.links, vec![(2, 1)]);
        assert_eq!(r.period_fills, vec![(1, "2026-Q3".to_string())]);
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn a_release_newer_than_the_page_row_wins() {
        let rows = [
            row(
                1,
                SOURCE_PAGE,
                Some("2026-Q3"),
                "2026-11-24",
                "2026-09-01T00:00:00Z",
            ),
            row(
                2,
                SOURCE_RELEASE,
                Some("2026-Q3"),
                "2026-11-20",
                "2026-10-01T00:00:00Z",
            ),
        ];
        let r = resolve_detailed(&rows, d(TODAY));
        assert_eq!(r.links, vec![(1, 2)]);
        assert_eq!(r.conflicts[0].winner, "release");
    }

    #[test]
    fn a_page_row_changed_after_the_release_wins() {
        let rows = [
            row(
                1,
                SOURCE_PAGE,
                Some("2026-Q3"),
                "2026-11-24",
                "2026-10-15T00:00:00Z",
            ),
            row(
                2,
                SOURCE_RELEASE,
                Some("2026-Q3"),
                "2026-11-20",
                "2026-10-01T00:00:00Z",
            ),
        ];
        let r = resolve_detailed(&rows, d(TODAY));
        assert_eq!(r.links, vec![(2, 1)]);
        assert_eq!(r.conflicts[0].winner, "page");
    }

    #[test]
    fn a_tie_goes_to_the_page() {
        let rows = [
            row(
                1,
                SOURCE_PAGE,
                Some("2026-Q3"),
                "2026-11-24",
                "2026-10-01T00:00:00Z",
            ),
            row(
                2,
                SOURCE_RELEASE,
                Some("2026-Q3"),
                "2026-11-20",
                "2026-10-01T00:00:00Z",
            ),
        ];
        assert_eq!(resolve(&rows, d(TODAY)), vec![(2, 1)]);
    }

    #[test]
    fn unknown_periods_are_the_same_report_within_the_window_only() {
        let page = row(1, SOURCE_PAGE, None, "2026-11-24", "2026-10-01T00:00:00Z");
        let near = row(
            2,
            SOURCE_RELEASE,
            None,
            "2026-11-29",
            "2026-10-02T00:00:00Z",
        );
        let far = row(
            3,
            SOURCE_RELEASE,
            None,
            "2026-12-04",
            "2026-10-02T00:00:00Z",
        );
        assert_eq!(resolve(&[page.clone(), near], d(TODAY)), vec![(1, 2)]);
        assert!(resolve(&[page, far], d(TODAY)).is_empty());
    }

    #[test]
    fn different_known_periods_or_kinds_are_different_reports() {
        let page = row(
            1,
            SOURCE_PAGE,
            Some("2026-Q3"),
            "2026-11-24",
            "2026-10-01T00:00:00Z",
        );
        let q2 = row(
            2,
            SOURCE_RELEASE,
            Some("2026-Q2"),
            "2026-11-25",
            "2026-10-02T00:00:00Z",
        );
        let mut annual = row(
            3,
            SOURCE_RELEASE,
            Some("2026"),
            "2026-11-25",
            "2026-10-02T00:00:00Z",
        );
        annual.report_kind = "annual".into();
        assert!(resolve(&[page.clone(), q2], d(TODAY)).is_empty());
        assert!(resolve(&[page, annual], d(TODAY)).is_empty());
    }

    #[test]
    fn rows_before_today_are_not_compared() {
        let rows = [
            row(
                1,
                SOURCE_PAGE,
                Some("2026-Q2"),
                "2026-10-20",
                "2026-09-01T00:00:00Z",
            ),
            row(
                2,
                SOURCE_RELEASE,
                Some("2026-Q2"),
                "2026-10-22",
                "2026-10-01T00:00:00Z",
            ),
        ];
        assert!(resolve(&rows, d(TODAY)).is_empty());
    }
}
