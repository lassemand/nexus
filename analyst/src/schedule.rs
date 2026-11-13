use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use sqlx::{PgPool, Row};
use tracing::warn;

use crate::store::Target;
use crate::types::ScheduledReport;

/// The instant a report is expected, for scheduling.
///
/// With a time of day, signal already stored the instant (`occurred_at`, the
/// Stockholm-local time converted to UTC). Without one, it is the start of
/// `expected_date` in the exchange's own time zone, the earliest the report
/// can come. `None` when there is no time and the zone is unknown or has no
/// midnight that day.
pub fn expected_at(
    expected_date: NaiveDate,
    time_known: bool,
    occurred_at: DateTime<Utc>,
    exchange_timezone: Option<&str>,
) -> Option<DateTime<Utc>> {
    if time_known {
        return Some(occurred_at);
    }
    let zone: Tz = exchange_timezone?.parse().ok()?;
    let midnight = expected_date.and_hms_opt(0, 0, 0)?;
    zone.from_local_datetime(&midnight)
        .earliest()
        .or_else(|| {
            zone.from_local_datetime(&(midnight + Duration::hours(1)))
                .earliest()
        })
        .map(|local| local.with_timezone(&Utc))
}

const WINNING_ROWS: &str = "\
    SELECT se.id, se.ticker, se.exchange_mic, \
           COALESCE(se.report_kind, CASE se.event_type WHEN 'annual_report' THEN 'annual' ELSE 'interim' END) AS kind, \
           se.fiscal_period, se.expected_date, (se.expected_time IS NOT NULL) AS time_known, \
           se.occurred_at, e.timezone \
    FROM special_events se \
    LEFT JOIN exchanges e ON e.mic = se.exchange_mic \
    WHERE se.status = 'scheduled' \
      AND se.superseded_by IS NULL \
      AND se.source IN ('mfn', 'mfn_release') \
      AND se.event_type IN ('quarterly_report', 'annual_report') \
      AND se.expected_date IS NOT NULL";

/// Reads signal's database. The pool must be read-only.
#[derive(Debug, Clone)]
pub struct SignalSchedule {
    pool: PgPool,
    allowlist: Vec<(String, String)>,
}

impl SignalSchedule {
    /// Reads through `pool`; a non-empty `allowlist` restricts the reports to
    /// those `(ticker, exchange_mic)` pairs.
    pub fn new(pool: PgPool, allowlist: Vec<(String, String)>) -> SignalSchedule {
        SignalSchedule { pool, allowlist }
    }

    fn allowed(&self, ticker: &str, mic: &str) -> bool {
        self.allowlist.is_empty() || self.allowlist.iter().any(|(t, m)| t == ticker && m == mic)
    }

    fn to_report(&self, row: &sqlx::postgres::PgRow) -> Option<ScheduledReport> {
        let id: i64 = row.try_get("id").ok()?;
        let ticker: String = row.try_get("ticker").ok()?;
        let exchange_mic: String = row.try_get("exchange_mic").ok()?;
        if !self.allowed(&ticker, &exchange_mic) {
            return None;
        }
        let kind: String = row.try_get("kind").ok()?;
        let expected_date: NaiveDate = row.try_get("expected_date").ok()?;
        let time_known: bool = row.try_get("time_known").ok()?;
        let occurred_at: DateTime<Utc> = row.try_get("occurred_at").ok()?;
        let timezone: Option<String> = row.try_get("timezone").ok()?;
        let report_kind = kind.parse().ok()?;
        let Some(expected_at) =
            expected_at(expected_date, time_known, occurred_at, timezone.as_deref())
        else {
            warn!(signal_event_id = id, %ticker, %exchange_mic, "cannot place the report in time: no time of day and no known exchange time zone");
            return None;
        };
        Some(ScheduledReport {
            signal_event_id: id,
            ticker,
            exchange_mic,
            report_kind,
            fiscal_period: row.try_get("fiscal_period").ok()?,
            expected_date,
            expected_time_known: time_known,
            expected_at,
        })
    }

    /// The winning scheduled reports with a date between `from` and `to`.
    pub async fn upcoming(
        &self,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<ScheduledReport>, sqlx::Error> {
        let sql = format!("{WINNING_ROWS} AND se.expected_date BETWEEN $1 AND $2 ORDER BY se.expected_date, se.id");
        let rows = sqlx::query(&sql)
            .bind(from)
            .bind(to)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().filter_map(|row| self.to_report(row)).collect())
    }

    /// The winning scheduled reports among `ids`, whatever their date.
    pub async fn by_ids(&self, ids: &[i64]) -> Result<Vec<ScheduledReport>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!("{WINNING_ROWS} AND se.id = ANY($1) ORDER BY se.id");
        let rows = sqlx::query(&sql).bind(ids).fetch_all(&self.pool).await?;
        Ok(rows.iter().filter_map(|row| self.to_report(row)).collect())
    }

    /// Whether `(ticker, exchange_mic)` is a registered company.
    pub async fn is_registered(
        &self,
        ticker: &str,
        exchange_mic: &str,
    ) -> Result<bool, sqlx::Error> {
        let found = sqlx::query("SELECT 1 FROM companies WHERE ticker = $1 AND exchange_mic = $2")
            .bind(ticker)
            .bind(exchange_mic)
            .fetch_optional(&self.pool)
            .await?;
        Ok(found.is_some())
    }
}

/// How targets and the schedule line up. Indexes refer to the slices given.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Matching {
    /// `(target, report)` pairs: the same report.
    pub pairs: Vec<(usize, usize)>,
    /// Reports no target follows yet.
    pub new_reports: Vec<usize>,
    /// Targets whose report is not in the schedule.
    pub missing_targets: Vec<usize>,
}

fn same_report(target: &Target, report: &ScheduledReport) -> bool {
    target.ticker == report.ticker
        && target.exchange_mic == report.exchange_mic
        && target.report_kind == report.report_kind.as_str()
        && target
            .fiscal_period
            .as_ref()
            .is_some_and(|period| Some(period) == report.fiscal_period.as_ref())
}

/// Decides which scheduled report each target follows.
///
/// A report belongs to a target when the `special_events` row id is the same
/// (it stays the same when a report's date moves), or else when ticker,
/// exchange, kind and fiscal period all match and the period is known (the row
/// id changes when the other calendar source starts to win). Every other
/// report is new.
pub fn match_reports(targets: &[Target], reports: &[ScheduledReport]) -> Matching {
    let mut matching = Matching::default();
    let mut report_taken = vec![false; reports.len()];
    let mut target_taken = vec![false; targets.len()];
    let rules: [fn(&Target, &ScheduledReport) -> bool; 2] = [
        |target, report| report.signal_event_id == target.signal_event_id,
        same_report,
    ];
    for rule in rules {
        for (ti, target) in targets.iter().enumerate() {
            if target_taken[ti] {
                continue;
            }
            let found =
                (0..reports.len()).find(|&ri| !report_taken[ri] && rule(target, &reports[ri]));
            if let Some(ri) = found {
                matching.pairs.push((ti, ri));
                report_taken[ri] = true;
                target_taken[ti] = true;
            }
        }
    }
    matching.new_reports = (0..reports.len()).filter(|ri| !report_taken[*ri]).collect();
    matching.missing_targets = (0..targets.len()).filter(|ti| !target_taken[*ti]).collect();
    matching
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::types::ReportKind;

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("valid date")
    }

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .expect("valid instant")
    }

    #[test]
    fn a_known_time_is_the_stored_instant() {
        let stored = utc(2026, 11, 20, 7, 0);
        assert_eq!(
            expected_at(date("2026-11-20"), true, stored, Some("Europe/Helsinki")),
            Some(stored)
        );
        assert_eq!(
            expected_at(date("2026-11-20"), true, stored, None),
            Some(stored)
        );
    }

    #[test]
    fn a_date_only_report_starts_at_midnight_in_the_exchange_zone() {
        assert_eq!(
            expected_at(
                date("2026-11-20"),
                false,
                utc(2026, 11, 19, 23, 0),
                Some("Europe/Helsinki")
            ),
            Some(utc(2026, 11, 19, 22, 0))
        );
        assert_eq!(
            expected_at(
                date("2026-07-01"),
                false,
                utc(2026, 6, 30, 22, 0),
                Some("Europe/Stockholm")
            ),
            Some(utc(2026, 6, 30, 22, 0))
        );
        assert_eq!(
            expected_at(
                date("2026-11-20"),
                false,
                utc(2026, 11, 20, 0, 0),
                Some("Europe/Copenhagen")
            ),
            Some(utc(2026, 11, 19, 23, 0))
        );
    }

    #[test]
    fn a_date_only_report_with_an_unknown_zone_cannot_be_placed() {
        assert_eq!(
            expected_at(date("2026-11-20"), false, utc(2026, 11, 20, 0, 0), None),
            None
        );
        assert_eq!(
            expected_at(
                date("2026-11-20"),
                false,
                utc(2026, 11, 20, 0, 0),
                Some("Not/AZone")
            ),
            None
        );
    }

    fn target(id: i64, event: i64, period: Option<&str>) -> Target {
        Target {
            id,
            ticker: "AAA".into(),
            exchange_mic: "XSTO".into(),
            report_kind: "interim".into(),
            fiscal_period: period.map(str::to_owned),
            expected_date: date("2026-11-20"),
            expected_time_known: true,
            expected_at: utc(2026, 11, 20, 7, 0),
            signal_event_id: event,
            status: "waiting".into(),
            prep_due_at: utc(2026, 11, 19, 19, 0),
            prep_attempts: 0,
            retry_after: None,
            prepared_for_at: None,
            created_at: utc(2026, 11, 1, 0, 0),
            updated_at: utc(2026, 11, 1, 0, 0),
        }
    }

    fn report(event: i64, period: Option<&str>) -> ScheduledReport {
        ScheduledReport {
            signal_event_id: event,
            ticker: "AAA".into(),
            exchange_mic: "XSTO".into(),
            report_kind: ReportKind::Interim,
            fiscal_period: period.map(str::to_owned),
            expected_date: date("2026-12-15"),
            expected_time_known: true,
            expected_at: utc(2026, 12, 15, 7, 0),
        }
    }

    #[test]
    fn the_row_id_matches_even_when_the_date_moved_far() {
        let matching = match_reports(&[target(1, 10, None)], &[report(10, None)]);
        assert_eq!(matching.pairs, vec![(0, 0)]);
        assert!(matching.new_reports.is_empty() && matching.missing_targets.is_empty());
    }

    #[test]
    fn a_new_row_with_the_same_period_is_the_same_report() {
        let matching = match_reports(
            &[target(1, 10, Some("Q3 2026"))],
            &[report(11, Some("Q3 2026"))],
        );
        assert_eq!(matching.pairs, vec![(0, 0)]);
    }

    #[test]
    fn a_new_row_without_a_period_is_a_new_report() {
        let matching = match_reports(&[target(1, 10, None)], &[report(11, None)]);
        assert!(matching.pairs.is_empty());
        assert_eq!(matching.new_reports, vec![0]);
        assert_eq!(matching.missing_targets, vec![0]);
    }

    #[test]
    fn a_different_period_is_a_new_report() {
        let matching = match_reports(
            &[target(1, 10, Some("Q3 2026"))],
            &[report(11, Some("Q4 2026"))],
        );
        assert!(matching.pairs.is_empty());
        assert_eq!(matching.new_reports, vec![0]);
    }

    #[test]
    fn another_company_or_kind_never_matches() {
        let mut other = report(11, Some("Q3 2026"));
        other.ticker = "BBB".into();
        assert!(match_reports(&[target(1, 10, Some("Q3 2026"))], &[other])
            .pairs
            .is_empty());
        let mut annual = report(11, Some("Q3 2026"));
        annual.report_kind = ReportKind::Annual;
        assert!(match_reports(&[target(1, 10, Some("Q3 2026"))], &[annual])
            .pairs
            .is_empty());
    }
}
