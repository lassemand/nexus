use std::str::FromStr;

use chrono::{DateTime, NaiveDate, Utc};

/// Whether a report is an interim (quarterly or half-year) or an annual one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReportKind {
    /// A quarterly or half-year report.
    Interim,
    /// A year-end report.
    Annual,
}

impl ReportKind {
    /// The value stored in the databases.
    pub fn as_str(self) -> &'static str {
        match self {
            ReportKind::Interim => "interim",
            ReportKind::Annual => "annual",
        }
    }
}

impl FromStr for ReportKind {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "interim" => Ok(ReportKind::Interim),
            "annual" => Ok(ReportKind::Annual),
            other => Err(format!("unknown report kind `{other}`")),
        }
    }
}

/// One upcoming report, as the winning scheduled row of signal's
/// `special_events` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledReport {
    /// The `special_events` row.
    pub signal_event_id: i64,
    /// The company's ticker.
    pub ticker: String,
    /// The company's exchange.
    pub exchange_mic: String,
    /// Interim or annual.
    pub report_kind: ReportKind,
    /// The fiscal period, when signal knows it.
    pub fiscal_period: Option<String>,
    /// The scheduled date.
    pub expected_date: NaiveDate,
    /// Whether the calendar gave a time of day.
    pub expected_time_known: bool,
    /// The instant used for scheduling.
    pub expected_at: DateTime<Utc>,
}

/// Where a report target is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetStatus {
    /// Waiting for its preparation to be due.
    Waiting,
    /// A preparation is running.
    Preparing,
    /// A kernel is accepted for it.
    Ready,
    /// Its preparations ran out of attempts or time.
    Failed,
    /// The report was released and decided.
    Decided,
    /// The report was released and skipped.
    Skipped,
    /// The report left the schedule.
    Cancelled,
}

impl TargetStatus {
    /// Every status, for metrics.
    pub const ALL: [TargetStatus; 7] = [
        TargetStatus::Waiting,
        TargetStatus::Preparing,
        TargetStatus::Ready,
        TargetStatus::Failed,
        TargetStatus::Decided,
        TargetStatus::Skipped,
        TargetStatus::Cancelled,
    ];

    /// The value stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            TargetStatus::Waiting => "waiting",
            TargetStatus::Preparing => "preparing",
            TargetStatus::Ready => "ready",
            TargetStatus::Failed => "failed",
            TargetStatus::Decided => "decided",
            TargetStatus::Skipped => "skipped",
            TargetStatus::Cancelled => "cancelled",
        }
    }
}

impl FromStr for TargetStatus {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        TargetStatus::ALL
            .into_iter()
            .find(|status| status.as_str() == text)
            .ok_or_else(|| format!("unknown target status `{text}`"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_status_and_kind_round_trips() {
        for status in TargetStatus::ALL {
            assert_eq!(status.as_str().parse(), Ok(status));
        }
        for kind in [ReportKind::Interim, ReportKind::Annual] {
            assert_eq!(kind.as_str().parse(), Ok(kind));
        }
        assert!("sleeping".parse::<TargetStatus>().is_err());
        assert!("weekly".parse::<ReportKind>().is_err());
    }
}
