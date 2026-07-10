/// Trading calendar support.
///
/// # Design
///
/// `model::calendar` is **data-free** — it defines the types and trait only.
/// Holiday dates are stored in Postgres (`trading_holidays` table) and loaded
/// at startup by the consuming binary. This means dates can be updated via SQL
/// without a code change or redeploy.
///
/// See `signal/migrations/*_create_trading_holidays.sql` for the schema and
/// seed data, and `signal::db::load_trading_holidays` for the loader.
///
/// # No US calendar equivalent
///
/// The codebase has no existing US/NYSE calendar module — this is the first
/// trading calendar implementation.
use chrono::{Datelike, NaiveDate, Weekday};
use std::collections::HashSet;

/// Classification of a given calendar date from a trading perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradingStatus {
    /// Regular full-session trading day.
    Open,
    /// Shortened session (early close).
    ///
    /// On Nasdaq Stockholm: Christmas Eve (Dec 24) and New Year's Eve (Dec 31)
    /// when they fall on a weekday (early close at 13:00 CET).
    HalfDay,
    /// Exchange closed — no trading. Includes weekends and public holidays.
    Closed,
}

/// A trading calendar that can classify any date.
pub trait TradingCalendar {
    /// Returns the trading status for the given date.
    fn trading_status(&self, date: NaiveDate) -> TradingStatus;

    /// Returns `true` if the exchange is open (full or half session) on `date`.
    fn is_trading_day(&self, date: NaiveDate) -> bool {
        self.trading_status(date) != TradingStatus::Closed
    }

    /// Returns `true` if `date` is a full-session day.
    fn is_full_session(&self, date: NaiveDate) -> bool {
        self.trading_status(date) == TradingStatus::Open
    }
}

/// A calendar backed by sets of dates loaded at runtime (e.g. from Postgres).
///
/// Construct via [`DynamicCalendar::new`], passing the holiday sets loaded
/// from the `trading_holidays` table for the relevant `exchange_mic`.
#[derive(Debug, Clone)]
pub struct DynamicCalendar {
    closed: HashSet<NaiveDate>,
    half_days: HashSet<NaiveDate>,
}

impl DynamicCalendar {
    /// Create a calendar from pre-loaded date sets.
    ///
    /// `closed` — dates the exchange is fully closed (weekday holidays).
    /// `half_days` — dates with an early close session.
    ///
    /// Weekend dates need not be included in either set — they are always
    /// treated as `Closed` regardless.
    pub fn new(closed: HashSet<NaiveDate>, half_days: HashSet<NaiveDate>) -> Self {
        Self { closed, half_days }
    }
}

impl TradingCalendar for DynamicCalendar {
    fn trading_status(&self, date: NaiveDate) -> TradingStatus {
        let wd = date.weekday();
        if wd == Weekday::Sat || wd == Weekday::Sun {
            return TradingStatus::Closed;
        }
        if self.closed.contains(&date) {
            return TradingStatus::Closed;
        }
        if self.half_days.contains(&date) {
            return TradingStatus::HalfDay;
        }
        TradingStatus::Open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn cal_with(closed: &[(i32, u32, u32)], half: &[(i32, u32, u32)]) -> DynamicCalendar {
        DynamicCalendar::new(
            closed.iter().map(|&(y, m, d)| date(y, m, d)).collect(),
            half.iter().map(|&(y, m, d)| date(y, m, d)).collect(),
        )
    }

    #[test]
    fn closed_date_is_closed() {
        let cal = cal_with(&[(2024, 3, 29)], &[]);
        assert_eq!(cal.trading_status(date(2024, 3, 29)), TradingStatus::Closed);
    }

    #[test]
    fn half_day_is_half_day() {
        let cal = cal_with(&[], &[(2024, 12, 24)]);
        assert_eq!(
            cal.trading_status(date(2024, 12, 24)),
            TradingStatus::HalfDay
        );
    }

    #[test]
    fn is_trading_day_true_for_half_days() {
        let cal = cal_with(&[], &[(2024, 12, 24)]);
        assert!(cal.is_trading_day(date(2024, 12, 24)));
    }

    #[test]
    fn regular_weekday_not_in_sets_is_open() {
        let cal = cal_with(&[], &[]);
        assert_eq!(cal.trading_status(date(2024, 9, 16)), TradingStatus::Open);
    }

    #[test]
    fn weekend_always_closed_regardless_of_sets() {
        let cal = cal_with(&[], &[]);
        assert_eq!(cal.trading_status(date(2024, 9, 14)), TradingStatus::Closed); // Sat
        assert_eq!(cal.trading_status(date(2024, 9, 15)), TradingStatus::Closed);
        // Sun
    }

    #[test]
    fn same_ticker_different_mic_are_separate_calendars() {
        // Two DynamicCalendar instances with different data are independent.
        // Jan 15 2024 = Monday, Jan 8 2024 = Monday (both weekdays).
        let us = cal_with(&[(2024, 1, 15)], &[]);
        let se = cal_with(&[(2024, 1, 8)], &[]);
        assert_eq!(us.trading_status(date(2024, 1, 15)), TradingStatus::Closed);
        assert_eq!(us.trading_status(date(2024, 1, 8)), TradingStatus::Open);
        assert_eq!(se.trading_status(date(2024, 1, 8)), TradingStatus::Closed);
        assert_eq!(se.trading_status(date(2024, 1, 15)), TradingStatus::Open);
    }
}
