//! Every fact about the Nordic countries we cover, one module per country.
//!
//! A country is a `const` [`Country`] holding data (currency, time zone, venues
//! with their session hours, Yahoo and Saxo identifiers, holiday rules, the
//! kind of insider source, the languages issuers publish in). [`COUNTRIES`] is
//! the one list of supported countries; the rest of the workspace derives its
//! MIC lists, time zones, calendars and Saxo ids from it.
//!
//! Rule of thumb: if two countries would write the same function body with
//! different constants, it is **data** here. If the code itself differs (how an
//! insider source is fetched, how a language writes a fiscal period) it is a
//! trait implementation in `chronicle`, keyed by [`InsiderSourceKind`] or
//! [`Lang`]. See `docs/guides/adding-a-nordic-country.md`.

use chrono::{Datelike, Duration, NaiveDate, NaiveTime};

pub mod dk;
pub mod fi;
pub mod se;

/// The time zone MFN publishes its financial calendar and release times in,
/// whatever the issuer's country. A macro so it can be spliced into SQL with
/// `concat!`; see [`MFN_TIMEZONE`].
#[macro_export]
macro_rules! mfn_timezone {
    () => {
        "Europe/Stockholm"
    };
}

/// The time zone of MFN's calendar (a property of the source, not of a country).
pub const MFN_TIMEZONE: &str = mfn_timezone!();

/// A language Nordic issuers publish their releases in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    /// Swedish.
    Sv,
    /// Danish.
    Da,
    /// Finnish.
    Fi,
    /// English.
    En,
}

/// What kind of source a country's insider transactions come from. The
/// implementation that fetches it lives in `chronicle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InsiderSourceKind {
    /// Finansinspektionen's insider register (Sweden).
    FiRegister,
    /// MAR Art. 19 managers' transaction releases on MFN (Finland).
    MarReleases,
    /// No insider source yet (Denmark).
    None,
}

/// A wall-clock time of day, constructible in a `const`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    /// Hour, 0-23.
    pub hour: u8,
    /// Minute, 0-59.
    pub minute: u8,
}

impl LocalTime {
    /// `hour:minute`.
    pub const fn new(hour: u8, minute: u8) -> Self {
        Self { hour, minute }
    }

    /// Minutes since midnight.
    pub const fn minutes(self) -> u32 {
        self.hour as u32 * 60 + self.minute as u32
    }

    /// As a [`NaiveTime`]; `None` for an out-of-range value.
    pub fn to_naive(self) -> Option<NaiveTime> {
        NaiveTime::from_hms_opt(u32::from(self.hour), u32::from(self.minute), 0)
    }
}

/// A trading venue (a MIC) and its session.
#[derive(Debug, Clone, Copy)]
pub struct Venue {
    /// ISO 10383 MIC; use the constants in [`crate::asset::mic`].
    pub mic: &'static str,
    /// Human-readable name.
    pub name: &'static str,
    /// Yahoo Finance `exchangeName` codes that resolve to this MIC.
    pub yahoo_exchange_codes: &'static [&'static str],
    /// Saxo `ExchangeId` for instrument lookup.
    pub saxo_exchange_id: &'static str,
    /// Whether `saxo_exchange_id` was checked against `GET /ref/v1/exchanges`.
    pub saxo_verified: bool,
    /// Start of the regular session, exchange-local.
    pub open_local: LocalTime,
    /// End of the regular session (including the closing auction), exchange-local.
    pub close_local: LocalTime,
    /// Close on a half day, `None` where the venue has none.
    pub half_day_close_local: Option<LocalTime>,
}

/// A closure computed from the year rather than read from Nager.Date.
#[derive(Debug, Clone, Copy)]
pub enum ExtraClosure {
    /// Same month/day every year.
    Fixed {
        /// Month, 1-12.
        month: u32,
        /// Day of month.
        day: u32,
        /// Label stored with the closure.
        note: &'static str,
    },
    /// A fixed number of days after Easter Sunday, optionally only up to and
    /// including `last_year`.
    EasterOffset {
        /// Days after Easter Sunday (negative: before).
        days: i64,
        /// Label stored with the closure.
        note: &'static str,
        /// Last year the closure applies to.
        last_year: Option<i32>,
    },
    /// The Friday between 19 and 25 June (Finnish Midsummer Eve).
    MidsummerEve {
        /// Label stored with the closure.
        note: &'static str,
    },
}

impl ExtraClosure {
    /// The date and label of this closure in `year`, `None` if it does not apply.
    pub fn resolve(&self, year: i32) -> Option<(NaiveDate, &'static str)> {
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
                Some((easter_sunday(year)? + Duration::days(days), note))
            }
            ExtraClosure::MidsummerEve { note } => {
                let first = NaiveDate::from_ymd_opt(year, 6, 19)?;
                let to_friday =
                    (4 - i64::from(first.weekday().num_days_from_monday())).rem_euclid(7);
                Some((first + Duration::days(to_friday), note))
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

/// How a country's trading holidays are derived from Nager.Date's public
/// holiday feed.
#[derive(Debug, Clone, Copy)]
pub struct HolidayRules {
    /// Holiday name substrings to skip (not observed by any exchange in the
    /// country, or always on a weekend).
    pub nager_skip: &'static [&'static str],
    /// Holiday name substrings that are early-close sessions, not closures.
    pub nager_half_days: &'static [&'static str],
    /// Closures the exchange observes that Nager.Date may not list. Added after
    /// the feed; a date the feed already produced is kept as is.
    pub extra_closures: &'static [ExtraClosure],
}

/// Everything we know about one country.
#[derive(Debug, Clone, Copy)]
pub struct Country {
    /// ISO 3166-1 alpha-2 code.
    pub code: &'static str,
    /// English name.
    pub name: &'static str,
    /// ISO 4217 currency of its venues.
    pub currency: &'static str,
    /// IANA time zone of its venues.
    pub timezone: &'static str,
    /// Its trading venues.
    pub venues: &'static [Venue],
    /// Trading-holiday rules.
    pub holidays: HolidayRules,
    /// Kind of insider-transaction source.
    pub insider_source: InsiderSourceKind,
    /// Languages its issuers publish in.
    pub languages: &'static [Lang],
}

/// The supported countries. Adding a country is one file and one line here.
pub const COUNTRIES: &[&Country] = &[&se::SWEDEN, &dk::DENMARK, &fi::FINLAND];

/// The country a MIC belongs to, `None` for non-Nordic or unknown MICs.
pub fn country_for_mic(mic: &str) -> Option<&'static Country> {
    COUNTRIES
        .iter()
        .copied()
        .find(|c| c.venues.iter().any(|v| v.mic == mic))
}

/// The country with ISO code `code`.
pub fn country_by_code(code: &str) -> Option<&'static Country> {
    COUNTRIES.iter().copied().find(|c| c.code == code)
}

/// The venue with MIC `mic`.
pub fn venue(mic: &str) -> Option<&'static Venue> {
    COUNTRIES
        .iter()
        .flat_map(|c| c.venues.iter())
        .find(|v| v.mic == mic)
}

/// Every Nordic MIC, in country then venue order.
pub fn nordic_mics() -> impl Iterator<Item = &'static str> {
    COUNTRIES
        .iter()
        .flat_map(|c| c.venues.iter())
        .map(|v| v.mic)
}

const fn venue_count() -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < COUNTRIES.len() {
        n += COUNTRIES[i].venues.len();
        i += 1;
    }
    n
}

const VENUE_COUNT: usize = venue_count();

const fn build_mics() -> [&'static str; VENUE_COUNT] {
    let mut out = [""; VENUE_COUNT];
    let (mut i, mut k) = (0, 0);
    while i < COUNTRIES.len() {
        let venues = COUNTRIES[i].venues;
        let mut j = 0;
        while j < venues.len() {
            out[k] = venues[j].mic;
            k += 1;
            j += 1;
        }
        i += 1;
    }
    out
}

/// [`nordic_mics`] as a constant array, for `const` slices such as `mic::NORDIC`.
pub const NORDIC_MICS: [&str; VENUE_COUNT] = build_mics();

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_mic_is_in_exactly_one_venue() {
        let all: Vec<&str> = nordic_mics().collect();
        let unique: HashSet<&str> = all.iter().copied().collect();
        assert_eq!(all.len(), unique.len(), "{all:?}");
        assert_eq!(all, NORDIC_MICS);
    }

    #[test]
    fn every_country_timezone_parses() {
        for c in COUNTRIES {
            assert!(c.timezone.parse::<chrono_tz::Tz>().is_ok(), "{}", c.code);
        }
    }

    #[test]
    fn currencies_are_three_letter_codes() {
        for c in COUNTRIES {
            assert!(
                c.currency.len() == 3 && c.currency.chars().all(|x| x.is_ascii_uppercase()),
                "{}",
                c.code
            );
        }
    }

    #[test]
    fn sessions_open_before_they_close_and_half_days_lie_between() {
        for v in COUNTRIES.iter().flat_map(|c| c.venues) {
            assert!(v.open_local.to_naive().is_some() && v.close_local.to_naive().is_some());
            assert!(
                v.open_local.minutes() < v.close_local.minutes(),
                "{}",
                v.mic
            );
            if let Some(h) = v.half_day_close_local {
                assert!(
                    v.open_local.minutes() < h.minutes() && h.minutes() < v.close_local.minutes(),
                    "{}",
                    v.mic
                );
            }
        }
    }

    #[test]
    fn no_two_countries_share_a_yahoo_code() {
        let mut seen = HashSet::new();
        for v in COUNTRIES.iter().flat_map(|c| c.venues) {
            for code in v.yahoo_exchange_codes {
                assert!(seen.insert(*code), "{code}");
            }
        }
    }

    #[test]
    fn country_codes_and_saxo_ids_are_unique_and_lookups_agree() {
        let codes: HashSet<_> = COUNTRIES.iter().map(|c| c.code).collect();
        assert_eq!(codes.len(), COUNTRIES.len());
        let saxo: HashSet<_> = COUNTRIES
            .iter()
            .flat_map(|c| c.venues)
            .map(|v| v.saxo_exchange_id)
            .collect();
        assert_eq!(saxo.len(), NORDIC_MICS.len());
        for c in COUNTRIES {
            for v in c.venues {
                assert_eq!(country_for_mic(v.mic).unwrap().code, c.code);
                assert_eq!(venue(v.mic).unwrap().mic, v.mic);
            }
        }
        assert!(country_for_mic("XOSL").is_none() && venue("XNAS").is_none());
    }

    #[test]
    fn easter_sundays_2023_to_2027() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
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
}
