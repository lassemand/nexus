use std::time::Duration;

use thiserror::Error;

/// How long a failed preparation waits before it is tried again.
pub const PREP_RETRY_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// The most a note may weigh, in bytes.
pub const MAX_NOTE_BYTES: usize = 4096;

/// A setting that is missing or unreadable.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ConfigError {
    /// A required variable is not set.
    #[error("{0} is required")]
    Missing(&'static str),
    /// A duration could not be read.
    #[error("{name}: {source}")]
    Duration {
        /// The variable.
        name: &'static str,
        /// What was wrong.
        source: humantime::DurationError,
    },
    /// A number could not be read, or is out of range.
    #[error("{name}: `{value}` is not a valid number")]
    Number {
        /// The variable.
        name: &'static str,
        /// What it held.
        value: String,
    },
    /// An `ANALYST_TICKERS` entry is not `TICKER:MIC`.
    #[error("ANALYST_TICKERS entry `{0}` is not TICKER:MIC")]
    Ticker(String),
}

/// The analyst's settings, read from the environment (see the crate README).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The analyst's own database.
    pub database_url: String,
    /// Signal's database, opened read-only.
    pub signal_database_url: String,
    /// Kafka bootstrap servers; the notes consumer needs them.
    pub kafka_brokers: String,
    /// How long before a report its preparation starts.
    pub prep_lead: Duration,
    /// A preparation that cannot finish this long before the report is not started.
    pub prep_min_lead: Duration,
    /// The most preparations running at once.
    pub prep_max_concurrent: u32,
    /// The most preparations started for one report in one cycle.
    pub prep_max_attempts: u32,
    /// How often the scheduler reads the schedule.
    pub scheduler_tick: Duration,
    /// When not empty, only these `(ticker, exchange_mic)` pairs are scheduled.
    pub analyst_tickers: Vec<(String, String)>,
    /// How long a kernel may run on a release.
    pub kernel_deadline: Duration,
    /// The Kafka topic of report notes.
    pub notes_topic: String,
}

impl Config {
    /// Reads the settings from the process environment.
    pub fn from_env() -> Result<Config, ConfigError> {
        Config::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads the settings through `lookup`, so tests need no environment.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
        let text = |name: &'static str| {
            lookup(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let required = |name: &'static str| text(name).ok_or(ConfigError::Missing(name));
        let duration = |name: &'static str, default: &str| {
            humantime::parse_duration(&text(name).unwrap_or_else(|| default.to_owned()))
                .map_err(|source| ConfigError::Duration { name, source })
        };
        let number = |name: &'static str, default: u32| match text(name) {
            None => Ok(default),
            Some(value) => value
                .parse::<u32>()
                .ok()
                .filter(|n| *n >= 1)
                .ok_or(ConfigError::Number { name, value }),
        };
        Ok(Config {
            database_url: required("DATABASE_URL")?,
            signal_database_url: required("SIGNAL_DATABASE_URL")?,
            kafka_brokers: required("KAFKA_BROKERS")?,
            prep_lead: duration("PREP_LEAD", "12h")?,
            prep_min_lead: duration("PREP_MIN_LEAD", "1h")?,
            prep_max_concurrent: number("PREP_MAX_CONCURRENT", 2)?,
            prep_max_attempts: number("PREP_MAX_ATTEMPTS", 3)?,
            scheduler_tick: duration("SCHEDULER_TICK", "60s")?,
            analyst_tickers: parse_tickers(&text("ANALYST_TICKERS").unwrap_or_default())?,
            kernel_deadline: duration("KERNEL_DEADLINE", "50ms")?,
            notes_topic: text("NOTES_TOPIC").unwrap_or_else(|| "analyst.notes".to_owned()),
        })
    }
}

/// Parses `TICKER:MIC,TICKER:MIC`; empty entries are skipped.
pub fn parse_tickers(text: &str) -> Result<Vec<(String, String)>, ConfigError> {
    text.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| match entry.split_once(':') {
            Some((ticker, mic)) if !ticker.trim().is_empty() && !mic.trim().is_empty() => {
                Ok((ticker.trim().to_owned(), mic.trim().to_owned()))
            }
            _ => Err(ConfigError::Ticker(entry.to_owned())),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn lookup(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<&str, &str> = pairs.iter().copied().collect();
        move |name| map.get(name).map(|v| (*v).to_owned())
    }

    const REQUIRED: [(&str, &str); 3] = [
        ("DATABASE_URL", "postgres://a"),
        ("SIGNAL_DATABASE_URL", "postgres://b"),
        ("KAFKA_BROKERS", "kafka:9092"),
    ];

    #[test]
    fn defaults() {
        let config = Config::from_lookup(lookup(&REQUIRED)).expect("valid");
        assert_eq!(config.prep_lead, Duration::from_secs(12 * 3600));
        assert_eq!(config.prep_min_lead, Duration::from_secs(3600));
        assert_eq!(config.prep_max_concurrent, 2);
        assert_eq!(config.prep_max_attempts, 3);
        assert_eq!(config.scheduler_tick, Duration::from_secs(60));
        assert_eq!(config.kernel_deadline, Duration::from_millis(50));
        assert_eq!(config.notes_topic, "analyst.notes");
        assert!(config.analyst_tickers.is_empty());
    }

    #[test]
    fn overrides() {
        let mut pairs = REQUIRED.to_vec();
        pairs.extend([
            ("PREP_LEAD", "1h30m"),
            ("PREP_MIN_LEAD", "45 min"),
            ("SCHEDULER_TICK", "1 minute 30 seconds"),
            ("PREP_MAX_CONCURRENT", "5"),
            ("ANALYST_TICKERS", "AAA:XSTO, BBB:XHEL,"),
            ("NOTES_TOPIC", "notes.test"),
            ("KERNEL_DEADLINE", "20ms"),
        ]);
        let config = Config::from_lookup(lookup(&pairs)).expect("valid");
        assert_eq!(config.prep_lead, Duration::from_secs(5400));
        assert_eq!(config.prep_min_lead, Duration::from_secs(45 * 60));
        assert_eq!(config.scheduler_tick, Duration::from_secs(90));
        assert_eq!(config.prep_max_concurrent, 5);
        assert_eq!(config.kernel_deadline, Duration::from_millis(20));
        assert_eq!(config.notes_topic, "notes.test");
        assert_eq!(
            config.analyst_tickers,
            vec![("AAA".into(), "XSTO".into()), ("BBB".into(), "XHEL".into())]
        );
    }

    #[test]
    fn missing_and_bad_values_are_errors() {
        assert_eq!(
            Config::from_lookup(lookup(&[])),
            Err(ConfigError::Missing("DATABASE_URL"))
        );
        let mut pairs = REQUIRED.to_vec();
        pairs.push(("PREP_LEAD", "soon"));
        assert!(matches!(
            Config::from_lookup(lookup(&pairs)),
            Err(ConfigError::Duration {
                name: "PREP_LEAD",
                ..
            })
        ));
        let mut pairs = REQUIRED.to_vec();
        pairs.push(("PREP_MAX_ATTEMPTS", "0"));
        assert!(matches!(
            Config::from_lookup(lookup(&pairs)),
            Err(ConfigError::Number {
                name: "PREP_MAX_ATTEMPTS",
                ..
            })
        ));
        let mut pairs = REQUIRED.to_vec();
        pairs.push(("ANALYST_TICKERS", "AAA"));
        assert_eq!(
            Config::from_lookup(lookup(&pairs)),
            Err(ConfigError::Ticker("AAA".into()))
        );
    }
}
