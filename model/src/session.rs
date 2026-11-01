//! Release sessions: where a publication instant falls relative to an
//! exchange's trading day. Pure (no I/O); shared by `signal` (price reactions)
//! and `chronicle` (`nordic-live`'s Slack messages), so both agree on the
//! definition. Callers load the `exchanges` and `trading_holidays` rows.
//!
//! The release instant is read in the exchange's own time zone. On a trading
//! day it is `PreOpen` before the open, `Intraday` until the close (the half-day
//! close on a half day, when the exchange has one) and `AfterClose` from then
//! on; on a weekend or a `closed` holiday it is `NonTradingDay`. A date outside
//! the holiday calendar's covered years has no session (`plan` returns `None`):
//! the calendar is never guessed.

use crate::calendar::{DynamicCalendar, TradingCalendar, TradingStatus};
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use chrono_tz::Tz;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use tracing::warn;

/// Where the release falls relative to the exchange's session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    PreOpen,
    Intraday,
    AfterClose,
    NonTradingDay,
}

impl Session {
    /// Value stored in `release_session`.
    pub fn as_str(self) -> &'static str {
        match self {
            Session::PreOpen => "pre_open",
            Session::Intraday => "intraday",
            Session::AfterClose => "after_close",
            Session::NonTradingDay => "non_trading_day",
        }
    }
}

/// One row of the `exchanges` table: where and when a MIC trades.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exchange {
    /// ISO 10383 MIC.
    pub mic: String,
    /// Country whose `trading_holidays` apply.
    pub country: String,
    /// The exchange's own IANA time zone.
    pub timezone: Tz,
    /// Regular session open, local time.
    pub open: NaiveTime,
    /// Regular session close, local time.
    pub close: NaiveTime,
    /// Early close on `half_day` dates, if the exchange has any.
    pub half_day_close: Option<NaiveTime>,
}

impl Exchange {
    /// The release instant in the exchange's local time.
    fn local(&self, at: DateTime<Utc>) -> NaiveDateTime {
        at.with_timezone(&self.timezone).naive_local()
    }

    /// The session close on a day with the given status.
    fn close_on(&self, status: TradingStatus) -> NaiveTime {
        match (status, self.half_day_close) {
            (TradingStatus::HalfDay, Some(early)) => early,
            (TradingStatus::HalfDay, None) => {
                // Logged once per MIC, not once per report.
                static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
                let first = WARNED
                    .get_or_init(Default::default)
                    .lock()
                    .map(|mut w| w.insert(self.mic.clone()))
                    .unwrap_or(false);
                if first {
                    warn!(mic = %self.mic,
                          "half_day in trading_holidays but the exchange has no half-day close; treated as a full day");
                }
                self.close
            }
            _ => self.close,
        }
    }
}

/// A country's trading calendar plus the date range it is known for.
#[derive(Debug, Clone)]
pub struct Calendar {
    days: DynamicCalendar,
    covered_from: NaiveDate,
    covered_to: NaiveDate,
}

impl Calendar {
    /// Builds a calendar from holiday rows. Coverage is the whole years from the
    /// first to the last holiday year. Returns `None` for no rows at all.
    pub fn from_holidays(
        closed: HashSet<NaiveDate>,
        half_days: HashSet<NaiveDate>,
    ) -> Option<Self> {
        let years: Vec<i32> = closed
            .iter()
            .chain(half_days.iter())
            .map(|d| d.year())
            .collect();
        let (first, last) = (*years.iter().min()?, *years.iter().max()?);
        Some(Self {
            covered_from: NaiveDate::from_ymd_opt(first, 1, 1)?,
            covered_to: NaiveDate::from_ymd_opt(last, 12, 31)?,
            days: DynamicCalendar::new(closed, half_days),
        })
    }

    fn covers(&self, d: NaiveDate) -> bool {
        d >= self.covered_from && d <= self.covered_to
    }

    fn is_trading_day(&self, d: NaiveDate) -> bool {
        self.days.is_trading_day(d)
    }

    /// The first trading day after `d`, if still inside the covered range.
    pub fn next_trading_day(&self, d: NaiveDate) -> Option<NaiveDate> {
        let mut day = d;
        loop {
            day = day.succ_opt()?;
            if !self.covers(day) {
                return None;
            }
            if self.is_trading_day(day) {
                return Some(day);
            }
        }
    }

    /// The last trading day before `d`, if still inside the covered range.
    pub fn prev_trading_day(&self, d: NaiveDate) -> Option<NaiveDate> {
        let mut day = d;
        loop {
            day = day.pred_opt()?;
            if !self.covers(day) {
                return None;
            }
            if self.is_trading_day(day) {
                return Some(day);
            }
        }
    }

    /// Trading days in `(after, up_to]`.
    pub fn trading_days_between(&self, after: NaiveDate, up_to: NaiveDate) -> i64 {
        let mut n = 0;
        let mut d = after;
        while d < up_to {
            d = d.succ_opt().expect("valid date");
            if self.is_trading_day(d) {
                n += 1;
            }
        }
        n
    }
}

/// The dates a release maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub session: Session,
    /// The exchange's local date of the release (`d`).
    pub release_date: NaiveDate,
    /// First trading day the market could react (`r`).
    pub reaction_date: NaiveDate,
    /// Last trading day before `r` (`b`).
    pub base_date: NaiveDate,
}

/// Classifies a release instant on `ex`. `None` means `calendar_unknown`: `d`,
/// `r` or `b` falls outside the calendar's covered range.
pub fn plan(occurred_at: DateTime<Utc>, cal: &Calendar, ex: &Exchange) -> Option<Plan> {
    let local = ex.local(occurred_at);
    let (d, t) = (local.date(), local.time());
    if !cal.covers(d) {
        return None;
    }
    let session = match cal.days.trading_status(d) {
        TradingStatus::Closed => Session::NonTradingDay,
        status => {
            if t < ex.open {
                Session::PreOpen
            } else if t < ex.close_on(status) {
                Session::Intraday
            } else {
                Session::AfterClose
            }
        }
    };
    let reaction_date = match session {
        Session::PreOpen | Session::Intraday => d,
        Session::AfterClose | Session::NonTradingDay => cal.next_trading_day(d)?,
    };
    Some(Plan {
        session,
        release_date: d,
        reaction_date,
        base_date: cal.prev_trading_day(reaction_date)?,
    })
}

/// The session of a release at `at`, or `None` when the calendar does not cover
/// the date (or the next/previous trading day needed by [`plan`]).
pub fn release_session(at: DateTime<Utc>, cal: &Calendar, ex: &Exchange) -> Option<Session> {
    plan(at, cal, ex).map(|p| p.session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn exchange(mic: &str, tz: Tz, close: NaiveTime, half: Option<NaiveTime>) -> Exchange {
        Exchange {
            mic: mic.into(),
            country: "XX".into(),
            timezone: tz,
            open: t(9, 0),
            close,
            half_day_close: half,
        }
    }

    fn calendar(closed: &[NaiveDate], half: &[NaiveDate]) -> Calendar {
        Calendar::from_holidays(
            closed.iter().copied().chain([d(2026, 1, 1)]).collect(),
            half.iter().copied().collect(),
        )
        .unwrap()
    }

    fn utc(y: i32, m: u32, day: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, day, h, mi, 0).unwrap()
    }

    #[test]
    fn sessions_follow_the_exchanges_own_hours() {
        let sto = exchange(
            "XSTO",
            chrono_tz::Europe::Stockholm,
            t(17, 30),
            Some(t(13, 0)),
        );
        let cph = exchange("XCSE", chrono_tz::Europe::Copenhagen, t(17, 0), None);
        let cal = calendar(&[d(2026, 12, 25)], &[d(2026, 12, 24)]);
        // Tue 24 Nov 2026, 07:30 CET = 06:30 UTC.
        assert_eq!(
            release_session(utc(2026, 11, 24, 6, 30), &cal, &sto),
            Some(Session::PreOpen)
        );
        assert_eq!(
            release_session(utc(2026, 11, 24, 11, 0), &cal, &sto),
            Some(Session::Intraday)
        );
        // 17:15 in Copenhagen is after its 17:00 close, 17:15 in Stockholm is not after 17:30.
        assert_eq!(
            release_session(utc(2026, 11, 24, 16, 15), &cal, &cph),
            Some(Session::AfterClose)
        );
        assert_eq!(
            release_session(utc(2026, 11, 24, 16, 15), &cal, &sto),
            Some(Session::Intraday)
        );
        // Half day (24 Dec): Stockholm closes at 13:00.
        assert_eq!(
            release_session(utc(2026, 12, 24, 12, 30), &cal, &sto),
            Some(Session::AfterClose)
        );
        // Holiday and weekend.
        assert_eq!(
            release_session(utc(2026, 12, 25, 9, 0), &cal, &sto),
            Some(Session::NonTradingDay)
        );
        assert_eq!(
            release_session(utc(2026, 11, 28, 9, 0), &cal, &sto),
            Some(Session::NonTradingDay)
        );
    }

    #[test]
    fn dates_outside_the_calendar_have_no_session() {
        let sto = exchange("XSTO", chrono_tz::Europe::Stockholm, t(17, 30), None);
        let cal = calendar(&[], &[]);
        assert_eq!(release_session(utc(2027, 6, 1, 8, 0), &cal, &sto), None);
    }
}
