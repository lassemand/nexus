/// Trading calendar support for Nasdaq Stockholm / First North Growth Market.
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
/// The holiday table covers years 2015–2030, sized to match the largest
/// lookback window in use: `chronicle/earnings_bin` defaults to
/// `LOOKBACK_YEARS=10`, which requires data back to approximately 2015.
///
/// # References
///
/// * Nasdaq Nordic official holiday schedule (published annually by Nasdaq)
/// * Swedish public holidays (Riksdagen SFS 1989:253)
use chrono::{Datelike, Duration, NaiveDate, Weekday};

/// Classification of a given calendar date from a trading perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradingStatus {
    /// Regular full-session trading day.
    Open,
    /// Shortened session (early close). Market opens normally but closes
    /// before the regular session end time.
    ///
    /// On Nasdaq Stockholm: Christmas Eve (Dec 24) and New Year's Eve
    /// (Dec 31) are early-close days when they fall on a weekday (13:00 CET).
    HalfDay,
    /// Exchange closed — no trading.
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

    /// Returns `true` if `date` is a full-session day (not a holiday and not
    /// an early-close day).
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
        stockholm_trading_status(date)
    }
}

/// Compute the trading status for a date on Nasdaq Stockholm.
fn stockholm_trading_status(date: NaiveDate) -> TradingStatus {
    let wd = date.weekday();

    // Weekends are always closed.
    if wd == Weekday::Sat || wd == Weekday::Sun {
        return TradingStatus::Closed;
    }

    let y = date.year();
    let m = date.month();
    let d = date.day();

    // New Year's Day
    if m == 1 && d == 1 {
        return TradingStatus::Closed;
    }

    // Easter-derived holidays
    let easter = easter_sunday(y);
    // Good Friday (Easter − 2 days)
    if date == easter - Duration::days(2) {
        return TradingStatus::Closed;
    }
    // Easter Monday (Easter + 1 day)
    if date == easter + Duration::days(1) {
        return TradingStatus::Closed;
    }
    // Ascension Thursday (Easter + 39 days)
    if date == easter + Duration::days(39) {
        return TradingStatus::Closed;
    }

    // Labour Day / May Day
    if m == 5 && d == 1 {
        return TradingStatus::Closed;
    }

    // National Day of Sweden (since 2005; observed when it falls on a weekday)
    if m == 6 && d == 6 {
        return TradingStatus::Closed;
    }

    // Midsummer Eve: the Friday immediately before Midsummer Day.
    // Midsummer Day (Midsommardag) is the Saturday between June 20–26.
    // Nasdaq Stockholm closes fully on Midsummer Eve — it is NOT a half day.
    if date == midsummer_eve(y) {
        return TradingStatus::Closed;
    }

    // Christmas Eve — early close (half day, 13:00 CET)
    if m == 12 && d == 24 {
        return TradingStatus::HalfDay;
    }

    // Christmas Day
    if m == 12 && d == 25 {
        return TradingStatus::Closed;
    }

    // Boxing Day (Dec 26)
    if m == 12 && d == 26 {
        return TradingStatus::Closed;
    }

    // New Year's Eve — early close (half day, 13:00 CET)
    if m == 12 && d == 31 {
        return TradingStatus::HalfDay;
    }

    TradingStatus::Open
}

/// Compute Easter Sunday for the given year using the Anonymous Gregorian
/// (Meeus/Jones/Butcher) algorithm.
fn easter_sunday(year: i32) -> NaiveDate {
    let a = year % 19;
    let b = year / 100;
    let c = year % 100;
    let d = b / 4;
    let e = b % 4;
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15) % 30;
    let i = c / 4;
    let k = c % 4;
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let m = (a + 11 * h + 22 * l) / 451;
    let month = (h + l - 7 * m + 114) / 31;
    let day = (h + l - 7 * m + 114) % 31 + 1;
    NaiveDate::from_ymd_opt(year, month as u32, day as u32)
        .expect("Easter algorithm produced an invalid date")
}

/// Compute Midsummer Eve for the given year.
///
/// Midsummer Day (Midsommardag) is the Saturday between June 20–26 (inclusive).
/// Midsummer Eve (Midsommarafton) is the Friday immediately preceding it.
fn midsummer_eve(year: i32) -> NaiveDate {
    // Find the Saturday in [June 20, June 26]
    let june_20 = NaiveDate::from_ymd_opt(year, 6, 20).unwrap();
    let days_until_saturday =
        (Weekday::Sat.num_days_from_monday() + 7 - june_20.weekday().num_days_from_monday()) % 7;
    let midsummer_day = june_20 + Duration::days(days_until_saturday as i64);
    // Midsummer Eve is the Friday before Midsummer Day
    midsummer_day - Duration::days(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    // ── Easter ────────────────────────────────────────────────────────────

    #[test]
    fn easter_2024_is_march_31() {
        assert_eq!(easter_sunday(2024), date(2024, 3, 31));
    }

    #[test]
    fn easter_2025_is_april_20() {
        assert_eq!(easter_sunday(2025), date(2025, 4, 20));
    }

    #[test]
    fn easter_2026_is_april_5() {
        assert_eq!(easter_sunday(2026), date(2026, 4, 5));
    }

    // ── Midsummer Eve ─────────────────────────────────────────────────────

    #[test]
    fn midsummer_eve_2024_is_june_21() {
        // Midsummer Day 2024 = June 22 (Saturday); Eve = June 21 (Friday)
        assert_eq!(midsummer_eve(2024), date(2024, 6, 21));
    }

    #[test]
    fn midsummer_eve_2025_is_june_20() {
        // Midsummer Day 2025 = June 21 (Saturday); Eve = June 20 (Friday)
        assert_eq!(midsummer_eve(2025), date(2025, 6, 20));
    }

    #[test]
    fn midsummer_eve_2026_is_june_19() {
        // Midsummer Day 2026 = June 20 (Saturday); Eve = June 19 (Friday)
        assert_eq!(midsummer_eve(2026), date(2026, 6, 19));
    }

    // ── Full closures 2024 ────────────────────────────────────────────────

    #[test]
    fn new_years_day_2024_closed() {
        let cal = StockholmCalendar;
        assert_eq!(cal.trading_status(date(2024, 1, 1)), TradingStatus::Closed);
    }

    #[test]
    fn good_friday_2024_closed() {
        // Easter 2024 = March 31 → Good Friday = March 29
        let cal = StockholmCalendar;
        assert_eq!(cal.trading_status(date(2024, 3, 29)), TradingStatus::Closed);
    }

    #[test]
    fn easter_monday_2024_closed() {
        // Easter 2024 = March 31 → Easter Monday = April 1
        let cal = StockholmCalendar;
        assert_eq!(cal.trading_status(date(2024, 4, 1)), TradingStatus::Closed);
    }

    #[test]
    fn ascension_2024_closed() {
        // Easter 2024 = March 31 → Ascension = May 9 (Thursday)
        let cal = StockholmCalendar;
        assert_eq!(cal.trading_status(date(2024, 5, 9)), TradingStatus::Closed);
    }

    #[test]
    fn may_day_2024_closed() {
        let cal = StockholmCalendar;
        assert_eq!(cal.trading_status(date(2024, 5, 1)), TradingStatus::Closed);
    }

    #[test]
    fn national_day_2024_closed() {
        // June 6, 2024 = Thursday
        let cal = StockholmCalendar;
        assert_eq!(cal.trading_status(date(2024, 6, 6)), TradingStatus::Closed);
    }

    #[test]
    fn midsummer_eve_2024_closed() {
        // June 21, 2024 = Friday
        let cal = StockholmCalendar;
        assert_eq!(cal.trading_status(date(2024, 6, 21)), TradingStatus::Closed);
    }

    #[test]
    fn christmas_day_2024_closed() {
        let cal = StockholmCalendar;
        assert_eq!(
            cal.trading_status(date(2024, 12, 25)),
            TradingStatus::Closed
        );
    }

    #[test]
    fn boxing_day_2024_closed() {
        let cal = StockholmCalendar;
        assert_eq!(
            cal.trading_status(date(2024, 12, 26)),
            TradingStatus::Closed
        );
    }

    // ── Half days 2024 ────────────────────────────────────────────────────

    #[test]
    fn christmas_eve_2024_is_half_day() {
        // Dec 24, 2024 = Tuesday — early close at 13:00 CET
        let cal = StockholmCalendar;
        assert_eq!(
            cal.trading_status(date(2024, 12, 24)),
            TradingStatus::HalfDay
        );
    }

    #[test]
    fn new_years_eve_2024_is_half_day() {
        // Dec 31, 2024 = Tuesday — early close at 13:00 CET
        let cal = StockholmCalendar;
        assert_eq!(
            cal.trading_status(date(2024, 12, 31)),
            TradingStatus::HalfDay
        );
    }

    // ── 2025 key dates ────────────────────────────────────────────────────

    #[test]
    fn good_friday_2025_closed() {
        // Easter 2025 = April 20 → Good Friday = April 18
        let cal = StockholmCalendar;
        assert_eq!(cal.trading_status(date(2025, 4, 18)), TradingStatus::Closed);
    }

    #[test]
    fn ascension_2025_closed() {
        // Easter 2025 = April 20 → Ascension = May 29 (Thursday)
        let cal = StockholmCalendar;
        assert_eq!(cal.trading_status(date(2025, 5, 29)), TradingStatus::Closed);
    }

    #[test]
    fn midsummer_eve_2025_closed() {
        // June 20, 2025 = Friday
        let cal = StockholmCalendar;
        assert_eq!(cal.trading_status(date(2025, 6, 20)), TradingStatus::Closed);
    }

    #[test]
    fn christmas_eve_2025_is_half_day() {
        // Dec 24, 2025 = Wednesday
        let cal = StockholmCalendar;
        assert_eq!(
            cal.trading_status(date(2025, 12, 24)),
            TradingStatus::HalfDay
        );
    }

    // ── 2026 key dates ────────────────────────────────────────────────────

    #[test]
    fn good_friday_2026_closed() {
        // Easter 2026 = April 5 → Good Friday = April 3
        let cal = StockholmCalendar;
        assert_eq!(cal.trading_status(date(2026, 4, 3)), TradingStatus::Closed);
    }

    #[test]
    fn ascension_2026_closed() {
        // Easter 2026 = April 5 → Ascension = May 14 (Thursday)
        let cal = StockholmCalendar;
        assert_eq!(cal.trading_status(date(2026, 5, 14)), TradingStatus::Closed);
    }

    #[test]
    fn midsummer_eve_2026_closed() {
        // June 19, 2026 = Friday
        let cal = StockholmCalendar;
        assert_eq!(cal.trading_status(date(2026, 6, 19)), TradingStatus::Closed);
    }

    #[test]
    fn national_day_weekend_is_open() {
        // June 6, 2026 = Saturday → no exchange effect; check nearest weekday
        let cal = StockholmCalendar;
        // June 8, 2026 = Monday — should be Open
        assert_eq!(cal.trading_status(date(2026, 6, 8)), TradingStatus::Open);
    }

    #[test]
    fn new_years_eve_2026_is_half_day() {
        // Dec 31, 2026 = Thursday
        let cal = StockholmCalendar;
        assert_eq!(
            cal.trading_status(date(2026, 12, 31)),
            TradingStatus::HalfDay
        );
    }

    // ── Trait helpers ─────────────────────────────────────────────────────

    #[test]
    fn is_trading_day_returns_true_for_half_days() {
        let cal = StockholmCalendar;
        // Christmas Eve is a half day but is_trading_day should be true
        assert!(cal.is_trading_day(date(2024, 12, 24)));
    }

    #[test]
    fn regular_weekday_is_open() {
        let cal = StockholmCalendar;
        // A typical Monday with no holiday
        assert_eq!(cal.trading_status(date(2024, 9, 16)), TradingStatus::Open);
    }

    #[test]
    fn weekend_is_closed() {
        let cal = StockholmCalendar;
        assert_eq!(cal.trading_status(date(2024, 9, 14)), TradingStatus::Closed); // Saturday
        assert_eq!(cal.trading_status(date(2024, 9, 15)), TradingStatus::Closed);
        // Sunday
    }
}
