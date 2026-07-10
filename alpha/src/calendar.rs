/// Trading calendar data provider.
///
/// Fetches public holiday data from the Nager.Date API
/// (`https://date.nager.at`) — free, no API key required — and maps it
/// to exchange-specific closure rules.
///
/// # Adding a new exchange
///
/// Add one entry to [`EXCHANGE_CALENDARS`]:
/// ```text
/// ExchangeCalendar {
///     mic:         "XHEL",
///     country:     "FI",
///     skip:        &["..."],
///     half_days:   &[],
/// }
/// ```
/// No code changes elsewhere required.
use chrono::{Datelike, NaiveDate, Weekday};
use serde::Deserialize;
use std::collections::HashSet;
use thiserror::Error;

/// Error type for calendar provider operations.
#[derive(Debug, Error)]
pub enum CalendarError {
    #[error("Nager.Date API request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("exchange {0} is not supported — add it to EXCHANGE_CALENDARS in alpha::calendar")]
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

// ── Static exchange → calendar mapping ───────────────────────────────────

/// Mapping from an exchange MIC to its Nager.Date country code and filtering rules.
struct ExchangeCalendar {
    /// ISO 10383 Market Identifier Code.
    mic: &'static str,
    /// ISO 3166-1 alpha-2 country code used by Nager.Date.
    country: &'static str,
    /// Holiday names (substring match) that the exchange does NOT observe.
    skip: &'static [&'static str],
    /// Holiday names (substring match) that are early-close (half day) rather
    /// than full closures.
    half_days: &'static [&'static str],
}

/// Static table of supported exchanges.
///
/// Multiple MICs can reference the same `country` + rules — e.g. XNYS and
/// XNAS both use the US NYSE calendar. Adding a new exchange is a single
/// entry here; no code changes elsewhere are needed.
const EXCHANGE_CALENDARS: &[ExchangeCalendar] = &[
    // ── Sweden ────────────────────────────────────────────────────────────
    ExchangeCalendar {
        mic: "FNSE",
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
    // ── United States (NYSE calendar) ─────────────────────────────────────
    // NYSE observes: New Year's Day, MLK Day, Presidents Day, Good Friday,
    // Memorial Day, Juneteenth (from 2021), Independence Day, Labor Day,
    // Thanksgiving, Christmas Day.
    ExchangeCalendar {
        mic: "XNYS",
        country: "US",
        skip: &[
            "Lincoln's Birthday",      // state holiday, not NYSE
            "Truman Day",              // Missouri state holiday
            "Columbus Day",            // not observed by NYSE
            "Indigenous Peoples' Day", // not observed by NYSE
            "Veterans Day",            // not observed by NYSE
        ],
        half_days: &[],
    },
    ExchangeCalendar {
        mic: "XNAS",
        country: "US",
        skip: &[
            "Lincoln's Birthday",
            "Truman Day",
            "Columbus Day",
            "Indigenous Peoples' Day",
            "Veterans Day",
        ],
        half_days: &[],
    },
];

// ── Provider ──────────────────────────────────────────────────────────────

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
        let cal = EXCHANGE_CALENDARS
            .iter()
            .find(|c| c.mic == exchange_mic)
            .ok_or_else(|| CalendarError::UnsupportedExchange(exchange_mic.to_string()))?;

        let raw = self.fetch(year, cal.country).await?;
        Ok(filter_holidays(raw, cal.skip, cal.half_days))
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

/// Apply exchange-specific filtering and half-day mapping to a raw holiday list.
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
