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
/// EXCHANGE_TO_COUNTRY    ──maps──►  exchange MIC → country code
/// ```
///
/// # Adding a new exchange
///
/// 1. If the exchange is in a new country: add a `CountryCalendar` entry to
///    [`COUNTRY_CALENDARS`] with the Nager.Date country code and filter rules.
/// 2. Add the exchange MIC and its country code to [`EXCHANGE_TO_COUNTRY`].
/// 3. Run `nexus calendar sync --country <CODE> --year <YEAR>` to seed the DB.
use chrono::{Datelike, NaiveDate, Weekday};
use serde::Deserialize;
use std::collections::HashSet;
use thiserror::Error;

/// Error type for calendar provider operations.
#[derive(Debug, Error)]
pub enum CalendarError {
    #[error("Nager.Date API request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("exchange MIC {0} not found in EXCHANGE_TO_COUNTRY")]
    UnknownExchange(String),

    #[error("country {0} not found in COUNTRY_CALENDARS")]
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
    /// Holiday name substrings to skip (not observed by any exchange in this
    /// country, or always falls on a weekend and has no weekday effect).
    skip: &'static [&'static str],
    /// Holiday name substrings that are early-close sessions rather than full
    /// closures.
    half_days: &'static [&'static str],
    /// Closures the exchange observes that Nager.Date may not list (or lists
    /// under another name/type). Added after the feed; a date the feed
    /// already produced is kept as is.
    extra_closures: &'static [ExtraClosure],
}

/// A closure computed from the year rather than read from Nager.Date.
enum ExtraClosure {
    /// Same month/day every year.
    Fixed {
        month: u32,
        day: u32,
        note: &'static str,
    },
    /// A fixed number of days after Easter Sunday, optionally only up to and
    /// including `last_year`.
    EasterOffset {
        days: i64,
        note: &'static str,
        last_year: Option<i32>,
    },
    /// The Friday between 19 and 25 June (Finnish Midsummer Eve).
    MidsummerEve { note: &'static str },
}

impl ExtraClosure {
    fn resolve(&self, year: i32) -> Option<(NaiveDate, &'static str)> {
        match *self {
            ExtraClosure::Fixed { month, day, note } => {
                NaiveDate::from_ymd_opt(year, month, day).map(|d| (d, note))
            }
            ExtraClosure::EasterOffset {
                days,
                note,
                last_year,
            } => {
                if last_year.is_some_and(|l| year > l) {
                    return None;
                }
                Some((easter_sunday(year)? + chrono::Duration::days(days), note))
            }
            ExtraClosure::MidsummerEve { note } => {
                let first = NaiveDate::from_ymd_opt(year, 6, 19)?;
                let to_friday =
                    (4 - i64::from(first.weekday().num_days_from_monday())).rem_euclid(7);
                Some((first + chrono::Duration::days(to_friday), note))
            }
        }
    }
}

/// Easter Sunday of `year` (Gregorian calendar, anonymous Gregorian algorithm).
pub fn easter_sunday(year: i32) -> Option<NaiveDate> {
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
}

/// Country-level calendar rules.
///
/// One entry per country. All exchanges in the same country share these rules.
const COUNTRY_CALENDARS: &[CountryCalendar] = &[
    CountryCalendar {
        country: "SE",
        skip: &[
            "Epiphany",        // Jan 6 — not observed by Nasdaq Stockholm
            "Easter Sunday",   // always a Sunday
            "Pentecost",       // always a Sunday
            "Whit Sunday",     // alternate name for Pentecost
            "Midsummer Day",   // the Saturday — Midsummer Eve (Friday) is the closure
            "All Saints' Day", // Nov 1 — not observed
        ],
        half_days: &[
            "Christmas Eve",  // Dec 24 — early close 13:00 CET
            "New Year's Eve", // Dec 31 — early close 13:00 CET
        ],
        extra_closures: &[],
    },
    // Nasdaq Copenhagen / First North Denmark: 09:00-17:00, no half days.
    // Nager.Date lists the bank holidays too, but the extra closures below
    // make the result independent of how it types them.
    CountryCalendar {
        country: "DK",
        skip: &[
            "Easter Sunday", // always a Sunday
            "Pentecost",     // always a Sunday
        ],
        half_days: &[],
        extra_closures: &[
            ExtraClosure::EasterOffset {
                days: 40,
                note: "Friday after Ascension Day",
                last_year: None,
            },
            ExtraClosure::Fixed {
                month: 6,
                day: 5,
                note: "Constitution Day",
            },
            ExtraClosure::Fixed {
                month: 12,
                day: 24,
                note: "Christmas Eve",
            },
            ExtraClosure::Fixed {
                month: 12,
                day: 31,
                note: "New Year's Eve",
            },
            // Store Bededag, abolished from 2024.
            ExtraClosure::EasterOffset {
                days: 26,
                note: "Great Prayer Day",
                last_year: Some(2023),
            },
        ],
    },
    // Nasdaq Helsinki / First North Finland: 10:00-18:30, no half days.
    // Nager.Date's FI list is not used as is: it has Saturday-only holidays
    // and Sundays, and lacks 24 and 31 December. The full closure set is
    // computed below, so the result does not depend on how Nager names or
    // types a date (a date it already lists is kept once).
    CountryCalendar {
        country: "FI",
        skip: &[
            "Easter Sunday",   // always a Sunday
            "Pentecost",       // always a Sunday
            "Whit Sunday",     // alternate name for Pentecost
            "Midsummer Day",   // the Saturday; Midsummer Eve is the closure
            "All Saints' Day", // a Saturday, not an exchange closure
        ],
        half_days: &[],
        extra_closures: &[
            ExtraClosure::Fixed {
                month: 1,
                day: 1,
                note: "New Year's Day",
            },
            ExtraClosure::Fixed {
                month: 1,
                day: 6,
                note: "Epiphany",
            },
            ExtraClosure::EasterOffset {
                days: -2,
                note: "Good Friday",
                last_year: None,
            },
            ExtraClosure::EasterOffset {
                days: 1,
                note: "Easter Monday",
                last_year: None,
            },
            ExtraClosure::Fixed {
                month: 5,
                day: 1,
                note: "May Day",
            },
            ExtraClosure::EasterOffset {
                days: 39,
                note: "Ascension Day",
                last_year: None,
            },
            ExtraClosure::MidsummerEve {
                note: "Midsummer Eve",
            },
            // Weekday-only: `filter_holidays` never stores a weekend date.
            ExtraClosure::Fixed {
                month: 12,
                day: 6,
                note: "Independence Day",
            },
            ExtraClosure::Fixed {
                month: 12,
                day: 24,
                note: "Christmas Eve",
            },
            ExtraClosure::Fixed {
                month: 12,
                day: 25,
                note: "Christmas Day",
            },
            ExtraClosure::Fixed {
                month: 12,
                day: 26,
                note: "Boxing Day",
            },
            ExtraClosure::Fixed {
                month: 12,
                day: 31,
                note: "New Year's Eve",
            },
        ],
    },
    CountryCalendar {
        country: "US",
        // NYSE/Nasdaq US observe: New Year's Day, MLK Day, Presidents Day,
        // Good Friday, Memorial Day, Juneteenth (from 2021), Independence Day,
        // Labor Day, Thanksgiving, Christmas Day.
        skip: &[
            "Lincoln's Birthday",      // state holiday, not NYSE
            "Truman Day",              // Missouri state holiday
            "Columbus Day",            // not observed by NYSE/Nasdaq
            "Indigenous Peoples' Day", // not observed by NYSE/Nasdaq
            "Veterans Day",            // not observed by NYSE/Nasdaq
        ],
        half_days: &[],
        extra_closures: &[],
    },
];

// ── Exchange → country mapping ────────────────────────────────────────────

/// Static mapping from exchange MIC to ISO 3166-1 alpha-2 country code.
///
/// Multiple MICs can share the same country (e.g. XNYS and XNAS both use
/// the US calendar). The country code determines which rows to read from
/// the `trading_holidays` table and which Nager.Date feed to fetch.
pub const EXCHANGE_TO_COUNTRY: &[(&str, &str)] = &[
    ("FNSE", "SE"), // Nasdaq First North Growth Market Stockholm
    ("XSTO", "SE"), // Nasdaq Stockholm
    ("XCSE", "DK"), // Nasdaq Copenhagen
    ("FNDK", "DK"), // Nasdaq First North Denmark
    ("XHEL", "FI"), // Nasdaq Helsinki
    ("FNFI", "FI"), // Nasdaq First North Finland
    ("XNYS", "US"), // New York Stock Exchange
    ("XNAS", "US"), // Nasdaq US
];

/// Resolve the country code for an exchange MIC.
///
/// Returns `None` if the MIC is not in [`EXCHANGE_TO_COUNTRY`].
pub fn country_for_exchange(mic: &str) -> Option<&'static str> {
    EXCHANGE_TO_COUNTRY
        .iter()
        .find(|(m, _)| *m == mic)
        .map(|(_, c)| *c)
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
        let cal = COUNTRY_CALENDARS
            .iter()
            .find(|c| c.country == country)
            .ok_or_else(|| CalendarError::UnknownCountry(country.to_string()))?;

        let raw = self.fetch(year, cal.country).await?;
        Ok(filter_holidays(
            raw,
            cal.skip,
            cal.half_days,
            cal.extra_closures,
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

    #[test]
    fn easter_sundays_2023_to_2027() {
        for (y, m, day) in [
            (2023, 4, 9),
            (2024, 3, 31),
            (2025, 4, 20),
            (2026, 4, 5),
            (2027, 3, 28),
        ] {
            assert_eq!(easter_sunday(y), Some(d(y, m, day)));
        }
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

    /// (mic, country) pairs from the `exchanges` seeds in the migrations.
    fn seeded_exchanges() -> Vec<(String, String)> {
        let sql = [
            include_str!("../../signal/migrations/20261027000000_create_exchanges.sql"),
            include_str!("../../signal/migrations/20261028000000_exchanges_add_finland.sql"),
        ]
        .join("\n");
        sql.lines()
            .filter_map(|l| {
                let l = l.trim().trim_start_matches('(');
                let parts: Vec<_> = l.split(',').map(|p| p.trim().trim_matches('\'')).collect();
                (parts.len() > 2 && parts[0].len() == 4 && l.contains("Europe/"))
                    .then(|| (parts[0].to_string(), parts[1].to_string()))
            })
            .collect()
    }

    #[test]
    fn exchanges_seed_agrees_with_exchange_to_country() {
        let seeded = seeded_exchanges();
        assert_eq!(
            seeded.len(),
            6,
            "FNSE, XSTO, XCSE, FNDK, XHEL, FNFI: {seeded:?}"
        );
        for (mic, country) in &seeded {
            assert_eq!(country_for_exchange(mic), Some(country.as_str()), "{mic}");
        }
        for mic in ["FNSE", "XSTO", "XCSE", "FNDK", "XHEL", "FNFI"] {
            assert!(seeded.iter().any(|(m, _)| m == mic), "{mic} not seeded");
        }
    }
}
