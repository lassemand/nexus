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

/// Fetches trading holiday data from the Nager.Date API for a given country
/// and year, applying the country-specific filtering rules.
pub struct CalendarProvider {
    client: reqwest::Client,
}

impl CalendarProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
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
        Ok(filter_holidays(raw, cal.skip, cal.half_days))
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
        let url = format!("https://date.nager.at/api/v3/publicholidays/{year}/{country}");
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
) -> Vec<CalendarEntry> {
    let mut seen_dates: HashSet<NaiveDate> = HashSet::new();
    raw.into_iter()
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
        .collect()
}

// ── Nager.Date wire types ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct NagerHoliday {
    date: NaiveDate,
    name: String,
}
