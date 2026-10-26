//! "Is any subscribed exchange open today?", for the price stream's heartbeat
//! and trading window.
//!
//! saxo-stream has no access to the signal database (`trading_holidays`), so the
//! closures come from the same source and rules that seed that table: the
//! country calendars of [`crate::calendar`] (Nager.Date plus the exchange's own
//! extra closures), fetched once at start. A country or year that could not be
//! loaded falls back to "weekdays are open", the safe direction: a missing
//! calendar never suppresses the heartbeat check.

use crate::calendar::{country_for_exchange, CalendarEntry, CalendarProvider};
use chrono::{Datelike, NaiveDate, Weekday};
use model::calendar::{DynamicCalendar, TradingCalendar};
use std::collections::{HashMap, HashSet};
use tracing::warn;

#[derive(Debug, Clone, Default)]
struct CountryDays {
    years: HashSet<i32>,
    closed: HashSet<NaiveDate>,
}

/// Closures per country, and which MICs map to which country.
#[derive(Debug, Clone, Default)]
pub struct MarketCalendar {
    countries: HashMap<String, CountryDays>,
    mic_country: HashMap<String, String>,
}

impl MarketCalendar {
    /// A calendar that knows nothing: every weekday counts as open.
    pub fn weekdays_only() -> Self {
        Self::default()
    }

    /// Loads the closures of every country the `mics` belong to, for `years`.
    /// A failed fetch is logged and leaves that country-year to the weekday rule.
    pub async fn load(provider: &CalendarProvider, mics: &[String], years: &[i32]) -> Self {
        let mut cal = Self::default();
        for mic in mics {
            if let Some(country) = country_for_exchange(mic) {
                cal.mic_country.insert(mic.clone(), country.to_string());
            }
        }
        let countries: HashSet<String> = cal.mic_country.values().cloned().collect();
        for country in countries {
            for &year in years {
                match provider.holidays_for_country(&country, year).await {
                    Ok(entries) => cal.add(&country, year, &entries),
                    Err(e) => warn!(country, year, error = %e,
                        "could not load trading holidays, weekdays count as open"),
                }
            }
        }
        cal
    }

    /// Registers `entries` as the complete closure list of `country` for `year`
    /// (and the MIC → country map is separate: see [`Self::with_mic`]).
    pub fn add(&mut self, country: &str, year: i32, entries: &[CalendarEntry]) {
        let days = self.countries.entry(country.to_string()).or_default();
        days.years.insert(year);
        days.closed.extend(
            entries
                .iter()
                .filter(|e| e.status == "closed")
                .map(|e| e.date),
        );
    }

    /// Declares that `mic` follows `country`'s calendar.
    pub fn with_mic(mut self, mic: &str, country: &str) -> Self {
        self.mic_country
            .insert(mic.to_string(), country.to_string());
        self
    }

    /// Whether the exchange `mic` is open on `date`.
    pub fn is_open(&self, mic: &str, date: NaiveDate) -> bool {
        let weekday = !matches!(date.weekday(), Weekday::Sat | Weekday::Sun);
        let known = self
            .mic_country
            .get(mic)
            .and_then(|c| self.countries.get(c))
            .filter(|d| d.years.contains(&date.year()));
        match known {
            Some(days) => {
                DynamicCalendar::new(days.closed.clone(), HashSet::new()).is_trading_day(date)
            }
            None => weekday,
        }
    }

    /// Whether at least one of `mics` is open on `date`. With no `mics`, the
    /// weekday rule applies.
    pub fn any_open<'a>(&self, mics: impl IntoIterator<Item = &'a str>, date: NaiveDate) -> bool {
        let mut any = false;
        for m in mics {
            any = true;
            if self.is_open(m, date) {
                return true;
            }
        }
        !any && !matches!(date.weekday(), Weekday::Sat | Weekday::Sun)
    }
}

impl MarketCalendar {
    /// [`Self::any_open`] at the instant `now`, each exchange judged on its own
    /// local date (an unknown MIC uses the UTC date).
    pub fn any_open_at<'a>(
        &self,
        mics: impl IntoIterator<Item = &'a str>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> bool {
        let mut any = false;
        for m in mics {
            any = true;
            let date = model::exchange::local_date(m, now).unwrap_or_else(|| now.date_naive());
            if self.is_open(m, date) {
                return true;
            }
        }
        !any && !matches!(now.weekday(), Weekday::Sat | Weekday::Sun)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    async fn calendar() -> MarketCalendar {
        let server = MockServer::start().await;
        // Denmark: Nager lists only public holidays; the Friday after Ascension
        // and Constitution Day come from the DK extra closures.
        Mock::given(method("GET"))
            .and(path("/api/v3/publicholidays/2026/DK"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"date": "2026-01-01", "name": "New Year's Day"},
                {"date": "2026-05-14", "name": "Ascension Day"},
            ])))
            .mount(&server)
            .await;
        // Sweden: Midsummer Eve is a closure; Ascension Friday is a normal day.
        Mock::given(method("GET"))
            .and(path("/api/v3/publicholidays/2026/SE"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"date": "2026-05-14", "name": "Ascension Day"},
                {"date": "2026-06-19", "name": "Midsummer Eve"},
            ])))
            .mount(&server)
            .await;
        let provider = CalendarProvider::with_base_url(server.uri());
        MarketCalendar::load(&provider, &["FNSE".into(), "XCSE".into()], &[2026]).await
    }

    #[tokio::test]
    async fn danish_only_holiday_does_not_count_when_only_copenhagen_is_subscribed() {
        let cal = calendar().await;
        let friday_after_ascension = d(2026, 5, 15);
        assert!(!cal.any_open(["XCSE"], friday_after_ascension));
        assert!(cal.any_open(["FNSE"], friday_after_ascension));
        // With both subscribed, Stockholm is open, so the heartbeat check stays on.
        assert!(cal.any_open(["FNSE", "XCSE"], friday_after_ascension));
    }

    #[tokio::test]
    async fn swedish_only_holiday_keeps_the_heartbeat_when_copenhagen_is_open() {
        let cal = calendar().await;
        let midsummer_eve = d(2026, 6, 19);
        assert!(!cal.any_open(["FNSE"], midsummer_eve));
        assert!(cal.any_open(["XCSE"], midsummer_eve));
        assert!(cal.any_open(["FNSE", "XCSE"], midsummer_eve));
    }

    #[tokio::test]
    async fn weekends_are_closed_and_unloaded_years_fall_back_to_weekdays() {
        let cal = calendar().await;
        assert!(!cal.any_open(["XCSE", "FNSE"], d(2026, 6, 20))); // Saturday
                                                                  // 2027 was not loaded: a weekday is treated as open, even Jan 1.
        assert!(cal.any_open(["XCSE"], d(2027, 1, 1)));
        assert!(MarketCalendar::weekdays_only().any_open(["XCSE"], d(2026, 5, 15)));
        assert!(!MarketCalendar::weekdays_only().any_open(std::iter::empty(), d(2026, 6, 20)));
    }

    #[tokio::test]
    async fn a_failed_fetch_leaves_the_weekday_rule() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let provider = CalendarProvider::with_base_url(server.uri());
        let cal = MarketCalendar::load(&provider, &["XCSE".into()], &[2026]).await;
        assert!(cal.any_open(["XCSE"], d(2026, 5, 15)));
    }
}
