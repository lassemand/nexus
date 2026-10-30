/// Trading calendar data provider.
///
/// Fetches public holiday data from the Nager.Date API
/// (`https://date.nager.at`) — free, no API key required — and stores
/// it as **country calendars** in Postgres. Exchanges map to a country
/// calendar; multiple exchanges in the same country share one calendar row.
///
/// # Data model
///
/// ```text
/// trading_holidays table  ──keyed by──►  (country, date)
///      ▲
///      │  resolved via
/// country_for_exchange   ──maps──►  exchange MIC → country code
/// ```
///
/// # Adding a new exchange
///
/// 1. Nordic: add the country file under `model/src/nordic/` (its
///    `HolidayRules` and venues) and a line in `COUNTRIES`; see
///    `docs/guides/adding-a-nordic-country.md`. Other countries need a
///    calendar here.
/// 2. Run `nexus calendar sync --country <CODE> --year <YEAR>` to seed the DB.
use chrono::{Datelike, NaiveDate, Weekday};
use model::nordic::{self, HolidayRules};
pub use model::nordic::{easter_sunday, ExtraClosure};
use serde::Deserialize;
use std::collections::HashSet;
use thiserror::Error;

/// Error type for calendar provider operations.
#[derive(Debug, Error)]
pub enum CalendarError {
    #[error("Nager.Date API request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("exchange MIC {0} has no country calendar")]
    UnknownExchange(String),

    #[error("country {0} has no calendar rules")]
    UnknownCountry(String),
}

/// A single trading-day exception fetched from an external calendar source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarEntry {
    pub date: NaiveDate,
    /// `"closed"` or `"half_day"`.
    pub status: &'static str,
    /// Human-readable label from the data source (e.g. `"Good Friday"`).
    pub note: String,
}

// ── Country calendar rules ────────────────────────────────────────────────

/// Filtering rules for a country's public holiday feed from Nager.Date.
struct CountryCalendar {
    /// ISO 3166-1 alpha-2 country code (matches Nager.Date API path).
    country: &'static str,
    rules: HolidayRules,
}

/// The US rules, the only calendar that does not come from [`model::nordic`].
const US_CALENDAR: CountryCalendar = CountryCalendar {
    country: "US",
    rules: HolidayRules {
        // NYSE/Nasdaq US observe: New Year's Day, MLK Day, Presidents Day,
        // Good Friday, Memorial Day, Juneteenth (from 2021), Independence Day,
        // Labor Day, Thanksgiving, Christmas Day.
        nager_skip: &[
            "Lincoln's Birthday",      // state holiday, not NYSE
            "Truman Day",              // Missouri state holiday
            "Columbus Day",            // not observed by NYSE/Nasdaq
            "Indigenous Peoples' Day", // not observed by NYSE/Nasdaq
            "Veterans Day",            // not observed by NYSE/Nasdaq
        ],
        nager_half_days: &[],
        extra_closures: &[],
    },
};

/// The calendar rules of `country`: Nordic ones from [`model::nordic`], else US.
fn calendar_for(country: &str) -> Option<CountryCalendar> {
    if let Some(c) = nordic::country_by_code(country) {
        return Some(CountryCalendar {
            country: c.code,
            rules: c.holidays,
        });
    }
    (country == US_CALENDAR.country).then_some(US_CALENDAR)
}

// ── Exchange → country mapping ────────────────────────────────────────────

/// Resolve the country code for an exchange MIC.
///
/// Nordic MICs come from [`model::nordic`]; XNYS and XNAS use the US calendar.
/// Returns `None` for any other MIC. The country code determines which rows to
/// read from the `trading_holidays` table and which Nager.Date feed to fetch.
pub fn country_for_exchange(mic: &str) -> Option<&'static str> {
    match mic {
        "XNYS" | "XNAS" => Some("US"),
        _ => nordic::country_for_mic(mic).map(|c| c.code),
    }
}

// ── Provider ──────────────────────────────────────────────────────────────

const NAGER_BASE_URL: &str = "https://date.nager.at";

/// Fetches trading holiday data from the Nager.Date API for a given country
/// and year, applying the country-specific filtering rules.
pub struct CalendarProvider {
    client: reqwest::Client,
    base_url: String,
}

impl CalendarProvider {
    pub fn new() -> Self {
        Self::with_base_url(NAGER_BASE_URL)
    }

    /// Same as [`Self::new`] but pointed at a different base URL — for
    /// tests to substitute a mock server instead of the real Nager.Date API.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            // A default (unset) reqwest client has no timeout at all — an
            // unresponsive (not just erroring) Nager.Date would hang forever
            // rather than fail cleanly. Matters a lot now that callers of
            // this (chronicle/src/verify.rs) run as a PR-gating smoke test:
            // a hang blocks CI far worse than a fast, clean failure does.
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            base_url: base_url.into(),
        }
    }

    /// Fetch trading exceptions for `country` in `year`.
    ///
    /// `country` is an ISO 3166-1 alpha-2 code (e.g. `"SE"`, `"US"`).
    /// Use [`country_for_exchange`] to resolve a MIC first if needed.
    pub async fn holidays_for_country(
        &self,
        country: &str,
        year: i32,
    ) -> Result<Vec<CalendarEntry>, CalendarError> {
        let cal = calendar_for(country)
            .ok_or_else(|| CalendarError::UnknownCountry(country.to_string()))?;

        let raw = self.fetch(year, cal.country).await?;
        Ok(filter_holidays(
            raw,
            cal.rules.nager_skip,
            cal.rules.nager_half_days,
            cal.rules.extra_closures,
            year,
        ))
    }

    /// Convenience: resolve `exchange_mic` to a country, then fetch.
    pub async fn holidays_for_exchange(
        &self,
        exchange_mic: &str,
        year: i32,
    ) -> Result<Vec<CalendarEntry>, CalendarError> {
        let country = country_for_exchange(exchange_mic)
            .ok_or_else(|| CalendarError::UnknownExchange(exchange_mic.to_string()))?;
        self.holidays_for_country(country, year).await
    }

    async fn fetch(&self, year: i32, country: &str) -> Result<Vec<NagerHoliday>, CalendarError> {
        let url = format!("{}/api/v3/publicholidays/{year}/{country}", self.base_url);
        Ok(self
            .client
            .get(&url)
            .header("User-Agent", "nexus-alpha")
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<NagerHoliday>>()
            .await?)
    }
}

impl Default for CalendarProvider {
    fn default() -> Self {
        Self::new()
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn filter_holidays(
    raw: Vec<NagerHoliday>,
    skip: &[&str],
    half_day_names: &[&str],
    extra_closures: &[ExtraClosure],
    year: i32,
) -> Vec<CalendarEntry> {
    let mut seen_dates: HashSet<NaiveDate> = HashSet::new();
    let mut entries: Vec<CalendarEntry> = raw
        .into_iter()
        .filter_map(|h| {
            if h.date.weekday() == Weekday::Sat || h.date.weekday() == Weekday::Sun {
                return None;
            }
            if skip.iter().any(|s| h.name.contains(s)) {
                return None;
            }
            if !seen_dates.insert(h.date) {
                return None;
            }
            let status = if half_day_names.iter().any(|s| h.name.contains(s)) {
                "half_day"
            } else {
                "closed"
            };
            Some(CalendarEntry {
                date: h.date,
                status,
                note: h.name,
            })
        })
        .collect();
    for (date, note) in extra_closures.iter().filter_map(|e| e.resolve(year)) {
        if date.weekday() != Weekday::Sat
            && date.weekday() != Weekday::Sun
            && seen_dates.insert(date)
        {
            entries.push(CalendarEntry {
                date,
                status: "closed",
                note: note.to_string(),
            });
        }
    }
    entries.sort_by_key(|e| e.date);
    entries
}

// ── Nager.Date wire types ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct NagerHoliday {
    date: NaiveDate,
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    async fn dk(year: i32, body: serde_json::Value) -> Vec<CalendarEntry> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/api/v3/publicholidays/{year}/DK")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        CalendarProvider::with_base_url(server.uri())
            .holidays_for_country("DK", year)
            .await
            .unwrap()
    }

    /// Nager's public holidays only: none of the bank holidays.
    fn public_2026() -> serde_json::Value {
        serde_json::json!([
            {"date": "2026-01-01", "name": "New Year's Day"},
            {"date": "2026-04-02", "name": "Maundy Thursday"},
            {"date": "2026-04-03", "name": "Good Friday"},
            {"date": "2026-04-05", "name": "Easter Sunday"},
            {"date": "2026-04-06", "name": "Easter Monday"},
            {"date": "2026-05-14", "name": "Ascension Day"},
            {"date": "2026-05-24", "name": "Pentecost"},
            {"date": "2026-05-25", "name": "Whit Monday"},
            {"date": "2026-12-25", "name": "Christmas Day"},
            {"date": "2026-12-26", "name": "St. Stephen's Day"},
        ])
    }

    #[tokio::test]
    async fn dk_2026_has_exactly_the_nasdaq_closures() {
        let got = dk(2026, public_2026()).await;
        let dates: Vec<_> = got.iter().map(|e| e.date).collect();
        assert_eq!(
            dates,
            vec![
                d(2026, 1, 1),
                d(2026, 4, 2),
                d(2026, 4, 3),
                d(2026, 4, 6),
                d(2026, 5, 14),
                d(2026, 5, 15),
                d(2026, 5, 25),
                d(2026, 6, 5),
                d(2026, 12, 24),
                d(2026, 12, 25),
                d(2026, 12, 31),
            ]
        );
        assert!(got.iter().all(|e| e.status == "closed"), "no half days");
    }

    #[tokio::test]
    async fn dk_does_not_duplicate_dates_nager_already_lists() {
        let mut body = public_2026();
        body.as_array_mut().unwrap().push(serde_json::json!(
            {"date": "2026-05-15", "name": "Bank closing day"}
        ));
        let got = dk(2026, body).await;
        assert_eq!(got.len(), 11);
    }

    #[tokio::test]
    async fn dk_2023_includes_great_prayer_day() {
        let body = serde_json::json!([
            {"date": "2023-01-01", "name": "New Year's Day"},
        ]);
        let got = dk(2023, body).await;
        assert!(got.iter().any(|e| e.date == d(2023, 5, 5)));
        assert!(got.iter().any(|e| e.date == d(2023, 5, 19)));
    }

    #[tokio::test]
    async fn dk_2024_has_no_great_prayer_day() {
        let got = dk(2024, serde_json::json!([])).await;
        // 4th Friday after Easter 2024 (Mar 31) is Apr 26.
        assert!(!got.iter().any(|e| e.date == d(2024, 4, 26)));
        assert!(got.iter().all(|e| e.status == "closed"));
    }

    #[test]
    fn danish_mics_map_to_dk() {
        assert_eq!(country_for_exchange("XCSE"), Some("DK"));
        assert_eq!(country_for_exchange("FNDK"), Some("DK"));
    }

    async fn fi(year: i32, body: serde_json::Value) -> Vec<CalendarEntry> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/api/v3/publicholidays/{year}/FI")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        CalendarProvider::with_base_url(server.uri())
            .holidays_for_country("FI", year)
            .await
            .unwrap()
    }

    /// What Nager.Date's FI feed looks like: Saturday/Sunday holidays and
    /// Easter Sunday/Pentecost included, 24 and 31 December missing.
    fn nager_fi_2026() -> serde_json::Value {
        serde_json::json!([
            {"date": "2026-01-01", "name": "New Year's Day"},
            {"date": "2026-01-06", "name": "Epiphany"},
            {"date": "2026-04-03", "name": "Good Friday"},
            {"date": "2026-04-05", "name": "Easter Sunday"},
            {"date": "2026-04-06", "name": "Easter Monday"},
            {"date": "2026-05-01", "name": "May Day"},
            {"date": "2026-05-14", "name": "Ascension Day"},
            {"date": "2026-05-24", "name": "Pentecost"},
            {"date": "2026-06-19", "name": "Midsummer Eve"},
            {"date": "2026-06-20", "name": "Midsummer Day"},
            {"date": "2026-10-31", "name": "All Saints' Day"},
            {"date": "2026-12-06", "name": "Independence Day"},
            {"date": "2026-12-25", "name": "Christmas Day"},
            {"date": "2026-12-26", "name": "St. Stephen's Day"},
        ])
    }

    fn dates(entries: &[CalendarEntry]) -> Vec<NaiveDate> {
        entries.iter().map(|e| e.date).collect()
    }

    #[tokio::test]
    async fn fi_2026_has_exactly_the_nasdaq_helsinki_closures() {
        let got = fi(2026, nager_fi_2026()).await;
        assert_eq!(
            dates(&got),
            vec![
                d(2026, 1, 1),
                d(2026, 1, 6),
                d(2026, 4, 3),
                d(2026, 4, 6),
                d(2026, 5, 1),
                d(2026, 5, 14),
                d(2026, 6, 19),
                d(2026, 12, 24),
                d(2026, 12, 25),
                d(2026, 12, 31),
            ]
        );
        assert!(got.iter().all(|e| e.status == "closed"), "no half days");
        // Not a closure on Nasdaq's list.
        assert!(!dates(&got).contains(&d(2026, 12, 28)));
    }

    #[tokio::test]
    async fn fi_is_complete_even_when_nager_lists_nothing() {
        assert_eq!(fi(2026, serde_json::json!([])).await.len(), 10);
    }

    #[tokio::test]
    async fn fi_does_not_duplicate_dates_nager_already_lists() {
        let mut body = nager_fi_2026();
        body.as_array_mut().unwrap().push(serde_json::json!(
            {"date": "2026-12-24", "name": "Christmas Eve"}
        ));
        let got = fi(2026, body).await;
        assert_eq!(got.len(), 10);
        let eve: Vec<_> = got.iter().filter(|e| e.date == d(2026, 12, 24)).collect();
        assert_eq!(eve.len(), 1);
        assert_eq!(eve[0].note, "Christmas Eve");
    }

    #[tokio::test]
    async fn fi_never_stores_weekend_dates() {
        for year in 2015..=2027 {
            let got = fi(year, serde_json::json!([])).await;
            assert!(
                got.iter()
                    .all(|e| !matches!(e.date.weekday(), Weekday::Sat | Weekday::Sun)),
                "{year}"
            );
        }
    }

    #[tokio::test]
    async fn fi_easter_dependent_closures_2023_to_2027() {
        // (year, Good Friday, Easter Monday, Ascension)
        for (y, gf, em, asc) in [
            (2023, d(2023, 4, 7), d(2023, 4, 10), d(2023, 5, 18)),
            (2024, d(2024, 3, 29), d(2024, 4, 1), d(2024, 5, 9)),
            (2025, d(2025, 4, 18), d(2025, 4, 21), d(2025, 5, 29)),
            (2026, d(2026, 4, 3), d(2026, 4, 6), d(2026, 5, 14)),
            (2027, d(2027, 3, 26), d(2027, 3, 29), d(2027, 5, 6)),
        ] {
            let got = dates(&fi(y, serde_json::json!([])).await);
            for day in [gf, em, asc] {
                assert!(got.contains(&day), "{y}: {day}");
            }
            // Easter Sunday and Whit Sunday are never stored.
            let easter = easter_sunday(y).unwrap();
            assert!(!got.contains(&easter));
            assert_eq!(gf, easter - chrono::Duration::days(2));
            assert_eq!(em, easter + chrono::Duration::days(1));
            assert_eq!(asc, easter + chrono::Duration::days(39));
        }
    }

    #[tokio::test]
    async fn fi_midsummer_eve_2023_to_2027() {
        for (y, day) in [
            (2023, d(2023, 6, 23)),
            (2024, d(2024, 6, 21)),
            (2025, d(2025, 6, 20)),
            (2026, d(2026, 6, 19)),
            (2027, d(2027, 6, 25)),
        ] {
            let got = fi(y, serde_json::json!([])).await;
            let eve: Vec<_> = got.iter().filter(|e| e.note == "Midsummer Eve").collect();
            assert_eq!(eve.len(), 1, "{y}");
            assert_eq!(eve[0].date, day, "{y}");
            assert_eq!(day.weekday(), Weekday::Fri);
        }
    }

    #[tokio::test]
    async fn fi_independence_day_and_boxing_day_close_only_on_weekdays() {
        // 2024: Dec 6 is a Friday, Dec 26 a Thursday.
        let y24 = dates(&fi(2024, serde_json::json!([])).await);
        assert!(y24.contains(&d(2024, 12, 6)));
        assert!(y24.contains(&d(2024, 12, 26)));
        // 2027: Dec 6 is a Monday; Dec 26 is a Sunday.
        let y27 = dates(&fi(2027, serde_json::json!([])).await);
        assert!(y27.contains(&d(2027, 12, 6)));
        assert!(!y27.contains(&d(2027, 12, 26)));
        // 2026: both fall on a weekend.
        let y26 = dates(&fi(2026, serde_json::json!([])).await);
        assert!(!y26.contains(&d(2026, 12, 6)));
        assert!(!y26.contains(&d(2026, 12, 26)));
    }

    /// Nasdaq's official 2027 list for Helsinki
    /// (nasdaq.com/european-market-activity/trading-hours).
    #[tokio::test]
    async fn fi_2027_matches_nasdaqs_published_list() {
        assert_eq!(
            dates(&fi(2027, serde_json::json!([])).await),
            vec![
                d(2027, 1, 1),
                d(2027, 1, 6),
                d(2027, 3, 26),
                d(2027, 3, 29),
                d(2027, 5, 6),
                d(2027, 6, 25),
                d(2027, 12, 6),
                d(2027, 12, 24),
                d(2027, 12, 31),
            ]
        );
    }

    #[test]
    fn finnish_mics_map_to_fi() {
        assert_eq!(country_for_exchange("XHEL"), Some("FI"));
        assert_eq!(country_for_exchange("FNFI"), Some("FI"));
    }
}
