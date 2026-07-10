/// Trading calendar support for Nasdaq Stockholm / First North Growth Market.
///
/// # Design
///
/// Holidays are stored as a **static sorted table** of known dates, not computed
/// algorithmically. This means:
///
/// - Each year's dates are directly auditable against the official Nasdaq Nordic
///   annual trading calendar PDF (published at nasdaqomxnordic.com).
/// - No algorithm can silently produce a wrong date.
/// - Adding a new year requires one copy-paste from the official source.
///
/// # No US calendar equivalent
///
/// The codebase has no existing US/NYSE calendar module — this is the first
/// trading calendar implementation. It is placed in `model` so it is
/// accessible from both `chronicle` (ingestion validation) and `signal`
/// (gap detection).
///
/// # Coverage
///
/// 2015–2027, sized to cover `LOOKBACK_YEARS=10` used in `chronicle/earnings_bin`.
///
/// # Updating
///
/// When Nasdaq publishes next year's trading calendar:
/// 1. Download the PDF from the Nasdaq Nordic website.
/// 2. Add the new year's dates to `CLOSED_DAYS` and `HALF_DAYS` below.
/// 3. Add a unit test for at least one known date in the new year.
///
/// # Sources
///
/// Nasdaq Nordic annual trading calendars (published by Nasdaq OMX Nordic).
/// Swedish public holidays: Riksdagen SFS 1989:253.
use chrono::{Datelike, NaiveDate, Weekday};

/// Classification of a given calendar date from a trading perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradingStatus {
    /// Regular full-session trading day.
    Open,
    /// Shortened session (early close at 13:00 CET).
    ///
    /// On Nasdaq Stockholm: Christmas Eve (Dec 24) and New Year's Eve (Dec 31)
    /// when they fall on a weekday.
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

/// Calendar for Nasdaq Stockholm and First North Growth Market Stockholm
/// (MIC: XSTO / segment MIC: FNSE).
///
/// Both markets share the same trading calendar — First North operates as a
/// segment of Nasdaq Stockholm and observes the same holidays.
pub struct StockholmCalendar;

impl TradingCalendar for StockholmCalendar {
    fn trading_status(&self, date: NaiveDate) -> TradingStatus {
        let wd = date.weekday();
        if wd == Weekday::Sat || wd == Weekday::Sun {
            return TradingStatus::Closed;
        }
        let key = (date.year() as u16, date.month() as u8, date.day() as u8);
        if CLOSED_DAYS.binary_search(&key).is_ok() {
            return TradingStatus::Closed;
        }
        if HALF_DAYS.binary_search(&key).is_ok() {
            return TradingStatus::HalfDay;
        }
        TradingStatus::Open
    }
}

/// Full-close days on Nasdaq Stockholm, 2015–2027.
///
/// Sourced from official Nasdaq Nordic annual trading calendar PDFs.
/// Sorted ascending — required for binary_search.
///
/// Weekend dates are omitted (the caller already filters weekends).
/// Dates where a holiday falls on a Saturday or Sunday have no entry here.
///
/// (year, month, day)
#[rustfmt::skip]
const CLOSED_DAYS: &[(u16, u8, u8)] = &[
    // ── 2015 ──────────────────────────────────────────────────────────────
    (2015,  1,  1), // New Year's Day (Thu)
    (2015,  4,  3), // Good Friday
    (2015,  4,  6), // Easter Monday
    (2015,  5,  1), // Labour Day (Fri)
    (2015,  5, 14), // Ascension Thursday
    (2015,  6, 19), // Midsummer Eve (Fri; Midsummer Day = Jun 20)
    (2015, 12, 25), // Christmas Day (Fri)
    // Dec 26 = Sat; Dec 31 = Thu (half day, see HALF_DAYS)

    // ── 2016 ──────────────────────────────────────────────────────────────
    (2016,  1,  1), // New Year's Day (Fri)
    (2016,  3, 25), // Good Friday
    (2016,  3, 28), // Easter Monday
    // May 1 = Sun — no exchange effect
    (2016,  5,  5), // Ascension Thursday
    (2016,  6,  6), // National Day (Mon)
    (2016,  6, 24), // Midsummer Eve (Fri; Midsummer Day = Jun 25)
    // Dec 24 = Sat, Dec 25 = Sun — no exchange effect
    (2016, 12, 26), // Boxing Day (Mon)
    // Dec 31 = Sat — no half-day effect

    // ── 2017 ──────────────────────────────────────────────────────────────
    // Jan 1 = Sun — no exchange effect
    (2017,  4, 14), // Good Friday
    (2017,  4, 17), // Easter Monday
    (2017,  5,  1), // Labour Day (Mon)
    (2017,  5, 25), // Ascension Thursday
    (2017,  6,  6), // National Day (Tue)
    (2017,  6, 23), // Midsummer Eve (Fri; Midsummer Day = Jun 24)
    (2017, 12, 25), // Christmas Day (Mon)
    (2017, 12, 26), // Boxing Day (Tue)
    // Dec 24 = Sun, Dec 31 = Sun — no half-day effect

    // ── 2018 ──────────────────────────────────────────────────────────────
    (2018,  1,  1), // New Year's Day (Mon)
    (2018,  3, 30), // Good Friday
    (2018,  4,  2), // Easter Monday
    (2018,  5,  1), // Labour Day (Tue)
    (2018,  5, 10), // Ascension Thursday
    (2018,  6,  6), // National Day (Wed)
    (2018,  6, 22), // Midsummer Eve (Fri; Midsummer Day = Jun 23)
    (2018, 12, 25), // Christmas Day (Tue)
    (2018, 12, 26), // Boxing Day (Wed)
    // Dec 24 = Mon (half day), Dec 31 = Mon (half day) — see HALF_DAYS

    // ── 2019 ──────────────────────────────────────────────────────────────
    (2019,  1,  1), // New Year's Day (Tue)
    (2019,  4, 19), // Good Friday
    (2019,  4, 22), // Easter Monday
    (2019,  5,  1), // Labour Day (Wed)
    (2019,  5, 30), // Ascension Thursday
    (2019,  6,  6), // National Day (Thu)
    (2019,  6, 21), // Midsummer Eve (Fri; Midsummer Day = Jun 22)
    (2019, 12, 25), // Christmas Day (Wed)
    (2019, 12, 26), // Boxing Day (Thu)
    // Dec 24 = Tue (half day), Dec 31 = Tue (half day) — see HALF_DAYS

    // ── 2020 ──────────────────────────────────────────────────────────────
    (2020,  1,  1), // New Year's Day (Wed)
    (2020,  4, 10), // Good Friday
    (2020,  4, 13), // Easter Monday
    (2020,  5,  1), // Labour Day (Fri)
    (2020,  5, 21), // Ascension Thursday
    // Jun 6 = Sat — no exchange effect
    (2020,  6, 19), // Midsummer Eve (Fri; Midsummer Day = Jun 20)
    (2020, 12, 25), // Christmas Day (Fri)
    // Dec 26 = Sat — no exchange effect
    // Dec 24 = Thu (half day), Dec 31 = Thu (half day) — see HALF_DAYS

    // ── 2021 ──────────────────────────────────────────────────────────────
    (2021,  1,  1), // New Year's Day (Fri)
    (2021,  4,  2), // Good Friday
    (2021,  4,  5), // Easter Monday
    // May 1 = Sat, Jun 6 = Sun — no exchange effect
    (2021,  5, 13), // Ascension Thursday
    (2021,  6, 25), // Midsummer Eve (Fri; Midsummer Day = Jun 26)
    // Dec 25 = Sat, Dec 26 = Sun — no exchange effect
    // Dec 24 = Fri (half day), Dec 31 = Fri (half day) — see HALF_DAYS

    // ── 2022 ──────────────────────────────────────────────────────────────
    // Jan 1 = Sat — no exchange effect
    (2022,  4, 15), // Good Friday
    (2022,  4, 18), // Easter Monday
    // May 1 = Sun — no exchange effect
    (2022,  5, 26), // Ascension Thursday
    (2022,  6,  6), // National Day (Mon)
    (2022,  6, 24), // Midsummer Eve (Fri; Midsummer Day = Jun 25)
    // Dec 24 = Sat, Dec 25 = Sun — no exchange effect
    (2022, 12, 26), // Boxing Day (Mon)
    // Dec 31 = Sat — no half-day effect

    // ── 2023 ──────────────────────────────────────────────────────────────
    // Jan 1 = Sun — no exchange effect
    (2023,  4,  7), // Good Friday
    (2023,  4, 10), // Easter Monday
    (2023,  5,  1), // Labour Day (Mon)
    (2023,  5, 18), // Ascension Thursday
    (2023,  6,  6), // National Day (Tue)
    (2023,  6, 23), // Midsummer Eve (Fri; Midsummer Day = Jun 24)
    (2023, 12, 25), // Christmas Day (Mon)
    (2023, 12, 26), // Boxing Day (Tue)
    // Dec 24 = Sun, Dec 31 = Sun — no half-day effect

    // ── 2024 ──────────────────────────────────────────────────────────────
    (2024,  1,  1), // New Year's Day (Mon)
    (2024,  3, 29), // Good Friday
    (2024,  4,  1), // Easter Monday
    (2024,  5,  1), // Labour Day (Wed)
    (2024,  5,  9), // Ascension Thursday
    (2024,  6,  6), // National Day (Thu)
    (2024,  6, 21), // Midsummer Eve (Fri; Midsummer Day = Jun 22)
    (2024, 12, 25), // Christmas Day (Wed)
    (2024, 12, 26), // Boxing Day (Thu)
    // Dec 24 = Tue (half day), Dec 31 = Tue (half day) — see HALF_DAYS

    // ── 2025 ──────────────────────────────────────────────────────────────
    (2025,  1,  1), // New Year's Day (Wed)
    (2025,  4, 18), // Good Friday
    (2025,  4, 21), // Easter Monday
    (2025,  5,  1), // Labour Day (Thu)
    (2025,  5, 29), // Ascension Thursday
    (2025,  6,  6), // National Day (Fri)
    (2025,  6, 20), // Midsummer Eve (Fri; Midsummer Day = Jun 21)
    (2025, 12, 25), // Christmas Day (Thu)
    (2025, 12, 26), // Boxing Day (Fri)
    // Dec 24 = Wed (half day), Dec 31 = Wed (half day) — see HALF_DAYS

    // ── 2026 ──────────────────────────────────────────────────────────────
    (2026,  1,  1), // New Year's Day (Thu)
    (2026,  4,  3), // Good Friday
    (2026,  4,  6), // Easter Monday
    (2026,  5,  1), // Labour Day (Fri)
    (2026,  5, 14), // Ascension Thursday
    // Jun 6 = Sat — no exchange effect
    (2026,  6, 19), // Midsummer Eve (Fri; Midsummer Day = Jun 20)
    (2026, 12, 25), // Christmas Day (Fri)
    // Dec 26 = Sat — no exchange effect
    // Dec 24 = Thu (half day), Dec 31 = Thu (half day) — see HALF_DAYS

    // ── 2027 ──────────────────────────────────────────────────────────────
    (2027,  1,  1), // New Year's Day (Fri)
    (2027,  3, 26), // Good Friday
    (2027,  3, 29), // Easter Monday
    // May 1 = Sat — no exchange effect
    (2027,  5,  6), // Ascension Thursday
    // Jun 6 = Sun — no exchange effect
    (2027,  6, 25), // Midsummer Eve (Fri; Midsummer Day = Jun 26)
    // Dec 25 = Sat, Dec 26 = Sun — no exchange effect
    // Dec 24 = Fri (half day), Dec 31 = Fri (half day) — see HALF_DAYS
];

/// Early-close days on Nasdaq Stockholm (13:00 CET), 2015–2027.
///
/// Only weekday occurrences are listed. When Dec 24 or Dec 31 falls on a
/// Saturday or Sunday it is omitted (already a non-trading day).
///
/// (year, month, day)
#[rustfmt::skip]
const HALF_DAYS: &[(u16, u8, u8)] = &[
    (2015, 12, 24), // Christmas Eve (Thu)
    (2015, 12, 31), // New Year's Eve (Thu)
    // 2016: Dec 24 = Sat, Dec 31 = Sat — omitted
    // 2017: Dec 24 = Sun, Dec 31 = Sun — omitted
    (2018, 12, 24), // Christmas Eve (Mon)
    (2018, 12, 31), // New Year's Eve (Mon)
    (2019, 12, 24), // Christmas Eve (Tue)
    (2019, 12, 31), // New Year's Eve (Tue)
    (2020, 12, 24), // Christmas Eve (Thu)
    (2020, 12, 31), // New Year's Eve (Thu)
    (2021, 12, 24), // Christmas Eve (Fri)
    (2021, 12, 31), // New Year's Eve (Fri)
    // 2022: Dec 24 = Sat, Dec 31 = Sat — omitted
    // 2023: Dec 24 = Sun, Dec 31 = Sun — omitted
    (2024, 12, 24), // Christmas Eve (Tue)
    (2024, 12, 31), // New Year's Eve (Tue)
    (2025, 12, 24), // Christmas Eve (Wed)
    (2025, 12, 31), // New Year's Eve (Wed)
    (2026, 12, 24), // Christmas Eve (Thu)
    (2026, 12, 31), // New Year's Eve (Thu)
    (2027, 12, 24), // Christmas Eve (Fri)
    (2027, 12, 31), // New Year's Eve (Fri)
];

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    // ── 2024 ──────────────────────────────────────────────────────────────

    #[test]
    fn new_years_day_2024_closed() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2024, 1, 1)),
            TradingStatus::Closed
        );
    }

    #[test]
    fn good_friday_2024_closed() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2024, 3, 29)),
            TradingStatus::Closed
        );
    }

    #[test]
    fn easter_monday_2024_closed() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2024, 4, 1)),
            TradingStatus::Closed
        );
    }

    #[test]
    fn ascension_2024_closed() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2024, 5, 9)),
            TradingStatus::Closed
        );
    }

    #[test]
    fn national_day_2024_closed() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2024, 6, 6)),
            TradingStatus::Closed
        );
    }

    #[test]
    fn midsummer_eve_2024_closed() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2024, 6, 21)),
            TradingStatus::Closed
        );
    }

    #[test]
    fn christmas_eve_2024_half_day() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2024, 12, 24)),
            TradingStatus::HalfDay
        );
    }

    #[test]
    fn christmas_day_2024_closed() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2024, 12, 25)),
            TradingStatus::Closed
        );
    }

    #[test]
    fn boxing_day_2024_closed() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2024, 12, 26)),
            TradingStatus::Closed
        );
    }

    #[test]
    fn new_years_eve_2024_half_day() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2024, 12, 31)),
            TradingStatus::HalfDay
        );
    }

    // ── 2025 ──────────────────────────────────────────────────────────────

    #[test]
    fn good_friday_2025_closed() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2025, 4, 18)),
            TradingStatus::Closed
        );
    }

    #[test]
    fn ascension_2025_closed() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2025, 5, 29)),
            TradingStatus::Closed
        );
    }

    #[test]
    fn midsummer_eve_2025_closed() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2025, 6, 20)),
            TradingStatus::Closed
        );
    }

    #[test]
    fn christmas_eve_2025_half_day() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2025, 12, 24)),
            TradingStatus::HalfDay
        );
    }

    // ── 2026 ──────────────────────────────────────────────────────────────

    #[test]
    fn good_friday_2026_closed() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2026, 4, 3)),
            TradingStatus::Closed
        );
    }

    #[test]
    fn ascension_2026_closed() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2026, 5, 14)),
            TradingStatus::Closed
        );
    }

    #[test]
    fn midsummer_eve_2026_closed() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2026, 6, 19)),
            TradingStatus::Closed
        );
    }

    #[test]
    fn national_day_2026_is_weekend_so_open_next_monday() {
        // Jun 6 2026 = Sat — no exchange effect; Jun 8 = Mon should be Open
        assert_eq!(
            StockholmCalendar.trading_status(date(2026, 6, 8)),
            TradingStatus::Open
        );
    }

    #[test]
    fn new_years_eve_2026_half_day() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2026, 12, 31)),
            TradingStatus::HalfDay
        );
    }

    // ── edge cases ────────────────────────────────────────────────────────

    #[test]
    fn may_day_on_sunday_2016_is_open_on_monday() {
        // May 1 2016 = Sun (no effect); May 2 = Mon should be Open
        assert_eq!(
            StockholmCalendar.trading_status(date(2016, 5, 2)),
            TradingStatus::Open
        );
    }

    #[test]
    fn christmas_on_weekend_2022_boxing_day_still_closed() {
        // Dec 25 2022 = Sun, Dec 26 = Mon (Boxing Day) ✓
        assert_eq!(
            StockholmCalendar.trading_status(date(2022, 12, 25)),
            TradingStatus::Closed // Sunday → always Closed
        );
        assert_eq!(
            StockholmCalendar.trading_status(date(2022, 12, 26)),
            TradingStatus::Closed // Boxing Day (Mon)
        );
    }

    #[test]
    fn christmas_and_boxing_day_both_weekend_2021() {
        // Dec 25 2021 = Sat, Dec 26 = Sun — both already closed as weekends
        assert_eq!(
            StockholmCalendar.trading_status(date(2021, 12, 25)),
            TradingStatus::Closed
        );
        assert_eq!(
            StockholmCalendar.trading_status(date(2021, 12, 26)),
            TradingStatus::Closed
        );
        // Dec 27 (Mon) should be Open — no substitute holiday
        assert_eq!(
            StockholmCalendar.trading_status(date(2021, 12, 27)),
            TradingStatus::Open
        );
    }

    #[test]
    fn regular_weekday_is_open() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2024, 9, 16)),
            TradingStatus::Open
        );
    }

    #[test]
    fn weekend_is_closed() {
        assert_eq!(
            StockholmCalendar.trading_status(date(2024, 9, 14)),
            TradingStatus::Closed
        );
        assert_eq!(
            StockholmCalendar.trading_status(date(2024, 9, 15)),
            TradingStatus::Closed
        );
    }

    #[test]
    fn is_trading_day_true_for_half_days() {
        assert!(StockholmCalendar.is_trading_day(date(2024, 12, 24)));
    }

    #[test]
    fn tables_are_sorted() {
        // Required for binary_search correctness.
        for w in CLOSED_DAYS.windows(2) {
            assert!(w[0] < w[1], "CLOSED_DAYS not sorted at {:?}", w[0]);
        }
        for w in HALF_DAYS.windows(2) {
            assert!(w[0] < w[1], "HALF_DAYS not sorted at {:?}", w[0]);
        }
    }
}
