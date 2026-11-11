use std::time::Duration;

use thiserror::Error;

/// A duration that could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DurationError {
    /// The text is empty.
    #[error("empty duration")]
    Empty,
    /// A number is missing before a unit, or a unit after a number.
    #[error("malformed duration `{0}`")]
    Malformed(String),
    /// The unit is not one of ns, us, ms, s, m, h, d, w (or the long names).
    #[error("unknown unit `{0}`")]
    UnknownUnit(String),
    /// The value does not fit.
    #[error("duration `{0}` is too large")]
    Overflow(String),
}

fn unit_nanos(unit: &str) -> Option<u128> {
    Some(match unit {
        "ns" | "nsec" | "nanos" | "nanosecond" | "nanoseconds" => 1,
        "us" | "µs" | "usec" | "micros" | "microsecond" | "microseconds" => 1_000,
        "ms" | "msec" | "millis" | "millisecond" | "milliseconds" => 1_000_000,
        "s" | "sec" | "secs" | "second" | "seconds" => 1_000_000_000,
        "m" | "min" | "mins" | "minute" | "minutes" => 60 * 1_000_000_000,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600 * 1_000_000_000,
        "d" | "day" | "days" => 86_400 * 1_000_000_000,
        "w" | "week" | "weeks" => 7 * 86_400 * 1_000_000_000,
        _ => return None,
    })
}

/// Parses a duration in the syntax of the `humantime` crate: whole numbers
/// each followed by a unit, optionally several in a row, `12h`, `1h30m`,
/// `1h 30m`, `50ms`, `5 min`. Units: `ns`, `us`, `ms`, `s`, `m`, `h`, `d`,
/// `w` and their long names.
pub fn parse_duration(text: &str) -> Result<Duration, DurationError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(DurationError::Empty);
    }
    let mut total: u128 = 0;
    let mut rest = text;
    while !rest.is_empty() {
        let digits_end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits_end == 0 {
            return Err(DurationError::Malformed(text.to_owned()));
        }
        let number: u128 = rest[..digits_end]
            .parse()
            .map_err(|_| DurationError::Overflow(text.to_owned()))?;
        rest = rest[digits_end..].trim_start();
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c.is_whitespace())
            .unwrap_or(rest.len());
        let unit = &rest[..unit_end];
        if !unit.chars().next().is_some_and(char::is_alphabetic) {
            return Err(DurationError::Malformed(text.to_owned()));
        }
        let nanos = unit_nanos(unit).ok_or_else(|| DurationError::UnknownUnit(unit.to_owned()))?;
        total = number
            .checked_mul(nanos)
            .and_then(|part| total.checked_add(part))
            .ok_or_else(|| DurationError::Overflow(text.to_owned()))?;
        rest = rest[unit_end..].trim_start();
    }
    let secs = u64::try_from(total / 1_000_000_000)
        .map_err(|_| DurationError::Overflow(text.to_owned()))?;
    Ok(Duration::new(secs, (total % 1_000_000_000) as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_units() {
        assert_eq!(parse_duration("12h"), Ok(Duration::from_secs(12 * 3600)));
        assert_eq!(parse_duration("60s"), Ok(Duration::from_secs(60)));
        assert_eq!(parse_duration("50ms"), Ok(Duration::from_millis(50)));
        assert_eq!(parse_duration("1h"), Ok(Duration::from_secs(3600)));
        assert_eq!(parse_duration("5min"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_duration("2d"), Ok(Duration::from_secs(2 * 86_400)));
        assert_eq!(parse_duration("1w"), Ok(Duration::from_secs(7 * 86_400)));
        assert_eq!(parse_duration("250us"), Ok(Duration::from_micros(250)));
    }

    #[test]
    fn several_units_with_or_without_spaces() {
        assert_eq!(parse_duration("1h30m"), Ok(Duration::from_secs(5400)));
        assert_eq!(parse_duration("1h 30m"), Ok(Duration::from_secs(5400)));
        assert_eq!(
            parse_duration("1 hour 30 minutes"),
            Ok(Duration::from_secs(5400))
        );
        assert_eq!(parse_duration("1s 500ms"), Ok(Duration::from_millis(1500)));
        assert_eq!(parse_duration("  12h "), Ok(Duration::from_secs(43_200)));
    }

    #[test]
    fn bad_input_is_an_error_not_a_guess() {
        assert_eq!(parse_duration(""), Err(DurationError::Empty));
        assert_eq!(parse_duration("   "), Err(DurationError::Empty));
        assert!(matches!(
            parse_duration("h"),
            Err(DurationError::Malformed(_))
        ));
        assert!(matches!(
            parse_duration("12"),
            Err(DurationError::Malformed(_))
        ));
        assert!(matches!(
            parse_duration("12x"),
            Err(DurationError::UnknownUnit(_))
        ));
        assert!(matches!(
            parse_duration("1.5h"),
            Err(DurationError::Malformed(_))
        ));
        assert!(matches!(
            parse_duration("-1h"),
            Err(DurationError::Malformed(_))
        ));
        assert!(matches!(
            parse_duration("99999999999999999999999999d"),
            Err(DurationError::Overflow(_))
        ));
    }
}
