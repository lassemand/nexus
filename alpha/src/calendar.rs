/// Trading calendar data provider.
///
/// Fetches public holiday data from the Nager.Date API
/// (`https://date.nager.at`) — free, no API key required — and maps it
/// to exchange-specific closure rules.
///
/// # Supported exchanges
///
/// | MIC   | Exchange                               | Country |
/// |-------|----------------------------------------|---------|
/// | FNSE  | Nasdaq First North Growth Market SE    | Sweden  |
use chrono::{Datelike, NaiveDate, Weekday};
use serde::Deserialize;
use thiserror::Error;

/// Error type for calendar provider operations.
#[derive(Debug, Error)]
pub enum CalendarError {
    #[error("Nager.Date API request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("exchange {0} is not supported by CalendarProvider")]
    UnsupportedExchange(String),
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

/// Fetches trading holiday data from the Nager.Date API for a given year and
/// exchange, applying exchange-specific filtering and half-day mapping.
pub struct CalendarProvider {
    client: reqwest::Client,
}

impl CalendarProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// Fetch trading exceptions for `exchange_mic` in `year`.
    ///
    /// Returns a list of [`CalendarEntry`] values for weekday dates only.
    /// Weekend occurrences are omitted — callers handle those separately.
    pub async fn holidays(
        &self,
        exchange_mic: &str,
        year: i32,
    ) -> Result<Vec<CalendarEntry>, CalendarError> {
        match exchange_mic {
            "FNSE" => self.fnse_holidays(year).await,
            other => Err(CalendarError::UnsupportedExchange(other.to_string())),
        }
    }

    async fn fnse_holidays(&self, year: i32) -> Result<Vec<CalendarEntry>, CalendarError> {
        let raw = self.fetch_se(year).await?;
        Ok(raw
            .into_iter()
            .filter_map(|h| {
                let wd = h.date.weekday();
                // Weekends are already non-trading days — omit them.
                if wd == Weekday::Sat || wd == Weekday::Sun {
                    return None;
                }
                // Nasdaq Stockholm does not close for these Swedish holidays.
                if FNSE_SKIP.iter().any(|s| h.name.contains(s)) {
                    return None;
                }
                let status = if FNSE_HALF_DAY.iter().any(|s| h.name.contains(s)) {
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
            .collect())
    }

    async fn fetch_se(&self, year: i32) -> Result<Vec<NagerHoliday>, CalendarError> {
        let url = format!("https://date.nager.at/api/v3/publicholidays/{year}/SE");
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

// ── Nager.Date wire types ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct NagerHoliday {
    date: NaiveDate,
    name: String,
}

// ── FNSE mapping rules ────────────────────────────────────────────────────

/// Swedish public holidays that Nasdaq Stockholm / First North (FNSE) does
/// NOT observe as a closure.
const FNSE_SKIP: &[&str] = &[
    "Epiphany",        // Jan 6 — not observed
    "Easter Sunday",   // always a Sunday
    "Pentecost",       // always a Sunday
    "Whit Sunday",     // alternate name for Pentecost
    "Midsummer Day",   // the Saturday — Midsummer Eve (Friday) is the closure
    "All Saints' Day", // Nov 1 — not observed
];

/// Holidays Nasdaq Stockholm observes as an early close (half day, 13:00 CET)
/// rather than a full closure.
const FNSE_HALF_DAY: &[&str] = &[
    "Christmas Eve",  // Dec 24
    "New Year's Eve", // Dec 31
];
