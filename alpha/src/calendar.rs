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
/// | XNYS  | New York Stock Exchange                | US      |
/// | XNAS  | Nasdaq US                              | US      |
use chrono::{Datelike, NaiveDate, Weekday};
use serde::Deserialize;
use std::collections::HashSet;
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
    /// Duplicate dates (e.g. Good Friday appearing as both national and
    /// state-level in the US feed) are deduplicated, keeping the first entry.
    pub async fn holidays(
        &self,
        exchange_mic: &str,
        year: i32,
    ) -> Result<Vec<CalendarEntry>, CalendarError> {
        match exchange_mic {
            "FNSE" => self.fnse_holidays(year).await,
            "XNYS" | "XNAS" => self.nyse_holidays(year).await,
            other => Err(CalendarError::UnsupportedExchange(other.to_string())),
        }
    }

    // ── FNSE ─────────────────────────────────────────────────────────────

    async fn fnse_holidays(&self, year: i32) -> Result<Vec<CalendarEntry>, CalendarError> {
        let raw = self.fetch(year, "SE").await?;
        Ok(filter_holidays(raw, FNSE_SKIP, FNSE_HALF_DAY))
    }

    // ── XNYS / XNAS ──────────────────────────────────────────────────────

    async fn nyse_holidays(&self, year: i32) -> Result<Vec<CalendarEntry>, CalendarError> {
        let raw = self.fetch(year, "US").await?;
        Ok(filter_holidays(raw, NYSE_SKIP, &[]))
    }

    // ── shared fetch ─────────────────────────────────────────────────────

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

/// Apply exchange-specific filtering and half-day mapping to a raw holiday list.
///
/// - Skips weekends (always non-trading).
/// - Skips holidays whose name contains any entry in `skip`.
/// - Maps to `half_day` if the name contains any entry in `half_day_names`.
/// - Deduplicates by date — the US feed has duplicate entries for the same
///   date (e.g. Good Friday appears as both national and state-level).
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
                return None; // duplicate date
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

// ── NYSE / XNAS mapping rules ─────────────────────────────────────────────

/// US public holidays that NYSE and Nasdaq US do NOT observe as closures.
///
/// NYSE closes for: New Year's Day, MLK Day, Presidents Day, Good Friday,
/// Memorial Day, Juneteenth, Independence Day, Labor Day, Thanksgiving,
/// Christmas Day.
///
/// Not observed: Lincoln's Birthday (state holiday), Truman Day (Missouri),
/// Columbus Day / Indigenous Peoples' Day, Veterans Day.
const NYSE_SKIP: &[&str] = &[
    "Lincoln's Birthday",      // state holiday, not a NYSE closure
    "Truman Day",              // Missouri state holiday
    "Columbus Day",            // not observed by NYSE
    "Indigenous Peoples' Day", // not observed by NYSE
    "Veterans Day",            // not observed by NYSE
];
