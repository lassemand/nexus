use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Europe::Stockholm;

use crate::store::{Store, Target, TARGET_COLUMNS};
use crate::types::{ReportKind, TargetStatus};

/// How far, in days, a release's date may be from a target's scheduled date for
/// the two to be the same report when the fiscal period cannot tell.
pub const NEAREST_DATE_DAYS: i64 = 5;

/// What the matching needs to know about a released report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The company's ticker.
    pub ticker: String,
    /// The company's exchange.
    pub exchange_mic: String,
    /// Interim or annual.
    pub kind: ReportKind,
    /// The release's fiscal period, when MFN gave one.
    pub fiscal_period: Option<String>,
    /// When MFN published it.
    pub published_at: DateTime<Utc>,
}

/// How a release was matched to its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// The same fiscal period.
    FiscalPeriod,
    /// The nearest scheduled date within [`NEAREST_DATE_DAYS`].
    NearestDate,
    /// No target.
    None,
}

impl MatchKind {
    /// The value of the `analyst.match` span attribute.
    pub fn as_str(self) -> &'static str {
        match self {
            MatchKind::FiscalPeriod => "fiscal_period",
            MatchKind::NearestDate => "nearest_date",
            MatchKind::None => "none",
        }
    }
}

/// The target of a release, and how it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matched {
    /// The target, if one matched.
    pub target: Option<Target>,
    /// How it matched.
    pub how: MatchKind,
}

fn settled(status: TargetStatus) -> bool {
    matches!(status, TargetStatus::Decided | TargetStatus::Skipped)
}

fn release_day(release: &Release) -> NaiveDate {
    release.published_at.with_timezone(&Stockholm).date_naive()
}

fn rank(target: &Target, day: NaiveDate) -> (bool, i64, NaiveDate, i64) {
    let status = target.status().ok();
    (
        status.is_some_and(settled),
        (target.expected_date - day).num_days().abs(),
        target.expected_date,
        target.id,
    )
}

fn best<'a>(targets: impl Iterator<Item = &'a Target>, day: NaiveDate) -> Option<&'a Target> {
    targets.min_by_key(|target| rank(target, day))
}

/// Picks the target of `release` among a company's targets.
///
/// Only targets of the same report kind and not cancelled count. First by the
/// same fiscal period (both set and equal). Otherwise the target whose
/// scheduled date is nearest to the release's date (in Stockholm, like the
/// calendar) within [`NEAREST_DATE_DAYS`], skipping targets whose fiscal period
/// differs from the release's when both are set. A target that is not yet
/// settled is preferred to one that is, so a late language version still finds
/// its report; decided and skipped targets are candidates so that a twin finds
/// the target that was already decided.
pub fn pick_target(candidates: &[Target], release: &Release) -> Matched {
    let day = release_day(release);
    let usable: Vec<&Target> = candidates
        .iter()
        .filter(|target| {
            target.kind().ok() == Some(release.kind)
                && target
                    .status()
                    .is_ok_and(|status| status != TargetStatus::Cancelled)
        })
        .collect();
    if let Some(period) = &release.fiscal_period {
        let same = usable
            .iter()
            .copied()
            .filter(|target| target.fiscal_period.as_deref() == Some(period.as_str()));
        if let Some(target) = best(same, day) {
            return Matched {
                target: Some(target.clone()),
                how: MatchKind::FiscalPeriod,
            };
        }
    }
    let near = usable.iter().copied().filter(|target| {
        let periods_differ = matches!(
            (&release.fiscal_period, &target.fiscal_period),
            (Some(a), Some(b)) if a != b
        );
        !periods_differ && (target.expected_date - day).num_days().abs() <= NEAREST_DATE_DAYS
    });
    match best(near, day) {
        Some(target) => Matched {
            target: Some(target.clone()),
            how: MatchKind::NearestDate,
        },
        None => Matched {
            target: None,
            how: MatchKind::None,
        },
    }
}

impl Store {
    /// Finds the target of a release among the company's targets.
    pub async fn match_target(&self, release: &Release) -> Result<Matched, sqlx::Error> {
        let candidates: Vec<Target> = sqlx::query_as(&format!(
            "SELECT {TARGET_COLUMNS} FROM report_targets
             WHERE ticker = $1 AND exchange_mic = $2 AND status <> 'cancelled'
             ORDER BY expected_at, id"
        ))
        .bind(&release.ticker)
        .bind(&release.exchange_mic)
        .fetch_all(&self.pool)
        .await?;
        Ok(pick_target(&candidates, release))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;

    fn day(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("valid date")
    }

    fn target(id: i64, kind: &str, period: Option<&str>, expected: &str, status: &str) -> Target {
        let at = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("valid instant");
        Target {
            id,
            ticker: "AAA".into(),
            exchange_mic: "XSTO".into(),
            report_kind: kind.into(),
            fiscal_period: period.map(str::to_owned),
            expected_date: day(expected),
            expected_time_known: true,
            expected_at: at,
            signal_event_id: id,
            status: status.into(),
            prep_due_at: at,
            prep_attempts: 0,
            retry_after: None,
            prepared_for_at: None,
            created_at: at,
            updated_at: at,
        }
    }

    fn release(period: Option<&str>, published: &str, kind: ReportKind) -> Release {
        Release {
            ticker: "AAA".into(),
            exchange_mic: "XSTO".into(),
            kind,
            fiscal_period: period.map(str::to_owned),
            published_at: Utc
                .with_ymd_and_hms(
                    published[0..4].parse().expect("year"),
                    published[5..7].parse().expect("month"),
                    published[8..10].parse().expect("day"),
                    7,
                    0,
                    0,
                )
                .single()
                .expect("valid instant"),
        }
    }

    fn id_of(matched: &Matched) -> Option<i64> {
        matched.target.as_ref().map(|target| target.id)
    }

    #[test]
    fn the_fiscal_period_beats_the_nearest_date() {
        let targets = [
            target(1, "interim", Some("2026-Q2"), "2026-08-20", "ready"),
            target(2, "interim", Some("2026-Q3"), "2026-11-05", "ready"),
        ];
        let matched = pick_target(
            &targets,
            &release(Some("2026-Q3"), "2026-08-20", ReportKind::Interim),
        );
        assert_eq!(id_of(&matched), Some(2));
        assert_eq!(matched.how, MatchKind::FiscalPeriod);
    }

    #[test]
    fn without_a_period_the_nearest_date_within_five_days_wins() {
        let targets = [
            target(1, "interim", None, "2026-11-01", "ready"),
            target(2, "interim", None, "2026-11-06", "waiting"),
        ];
        let matched = pick_target(&targets, &release(None, "2026-11-05", ReportKind::Interim));
        assert_eq!(id_of(&matched), Some(2));
        assert_eq!(matched.how, MatchKind::NearestDate);
    }

    #[test]
    fn a_report_two_weeks_early_matches_nothing() {
        let targets = [target(1, "interim", Some("2026-Q3"), "2026-11-20", "ready")];
        let early = pick_target(&targets, &release(None, "2026-11-06", ReportKind::Interim));
        assert_eq!(id_of(&early), None);
        assert_eq!(early.how, MatchKind::None);
        let early_with_period = pick_target(
            &targets,
            &release(Some("2026-Q4"), "2026-11-06", ReportKind::Interim),
        );
        assert_eq!(id_of(&early_with_period), None);
    }

    #[test]
    fn five_days_is_inside_and_six_is_outside() {
        let targets = [target(1, "interim", None, "2026-11-10", "ready")];
        assert_eq!(
            id_of(&pick_target(
                &targets,
                &release(None, "2026-11-05", ReportKind::Interim)
            )),
            Some(1)
        );
        assert_eq!(
            id_of(&pick_target(
                &targets,
                &release(None, "2026-11-15", ReportKind::Interim)
            )),
            Some(1)
        );
        assert_eq!(
            id_of(&pick_target(
                &targets,
                &release(None, "2026-11-04", ReportKind::Interim)
            )),
            None
        );
        assert_eq!(
            id_of(&pick_target(
                &targets,
                &release(None, "2026-11-16", ReportKind::Interim)
            )),
            None
        );
    }

    #[test]
    fn different_periods_are_different_reports_even_on_the_same_day() {
        let targets = [target(1, "interim", Some("2026-Q2"), "2026-11-05", "ready")];
        let matched = pick_target(
            &targets,
            &release(Some("2026-Q3"), "2026-11-05", ReportKind::Interim),
        );
        assert_eq!(id_of(&matched), None);
    }

    #[test]
    fn a_target_without_a_period_still_matches_a_release_with_one_by_date() {
        let targets = [target(1, "interim", None, "2026-11-05", "ready")];
        let matched = pick_target(
            &targets,
            &release(Some("2026-Q3"), "2026-11-05", ReportKind::Interim),
        );
        assert_eq!(id_of(&matched), Some(1));
        assert_eq!(matched.how, MatchKind::NearestDate);
    }

    #[test]
    fn the_kind_must_match() {
        let targets = [target(1, "annual", None, "2026-11-05", "ready")];
        assert_eq!(
            id_of(&pick_target(
                &targets,
                &release(None, "2026-11-05", ReportKind::Interim)
            )),
            None
        );
        assert_eq!(
            id_of(&pick_target(
                &targets,
                &release(None, "2026-11-05", ReportKind::Annual)
            )),
            Some(1)
        );
    }

    #[test]
    fn cancelled_targets_never_match() {
        let targets = [target(
            1,
            "interim",
            Some("2026-Q3"),
            "2026-11-05",
            "cancelled",
        )];
        let matched = pick_target(
            &targets,
            &release(Some("2026-Q3"), "2026-11-05", ReportKind::Interim),
        );
        assert_eq!(id_of(&matched), None);
    }

    #[test]
    fn a_settled_target_is_found_so_that_a_twin_can_be_recognised() {
        let targets = [target(
            1,
            "interim",
            Some("2026-Q3"),
            "2026-11-05",
            "decided",
        )];
        let matched = pick_target(
            &targets,
            &release(Some("2026-Q3"), "2026-11-05", ReportKind::Interim),
        );
        assert_eq!(id_of(&matched), Some(1));
        let by_date = pick_target(&targets, &release(None, "2026-11-06", ReportKind::Interim));
        assert_eq!(id_of(&by_date), Some(1));
    }

    #[test]
    fn an_unsettled_target_is_preferred_to_a_settled_one() {
        let targets = [
            target(1, "interim", None, "2026-11-05", "decided"),
            target(2, "interim", None, "2026-11-08", "ready"),
        ];
        let matched = pick_target(&targets, &release(None, "2026-11-05", ReportKind::Interim));
        assert_eq!(id_of(&matched), Some(2));
    }

    #[test]
    fn the_release_date_is_the_stockholm_date() {
        let targets = [target(1, "interim", None, "2026-11-10", "ready")];
        let mut late = release(None, "2026-11-04", ReportKind::Interim);
        late.published_at = Utc
            .with_ymd_and_hms(2026, 11, 4, 23, 30, 0)
            .single()
            .expect("valid instant");
        assert_eq!(
            id_of(&pick_target(&targets, &late)),
            Some(1),
            "00:30 on 5 November in Stockholm is 5 days before"
        );
        late.published_at -= Duration::hours(1);
        assert_eq!(
            id_of(&pick_target(&targets, &late)),
            None,
            "22:30 UTC is still 4 November in UTC and in Stockholm: 6 days"
        );
    }
}
