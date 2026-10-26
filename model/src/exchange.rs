//! Exchange-local time for the Nordic venues, without a time-zone database.
//!
//! The IANA zone names here are the same values as `exchanges.timezone` in the
//! signal database (a test in `alpha` keeps the two in step). Stockholm and
//! Copenhagen both follow the EU rule (CET/CEST), so one offset function serves
//! both; each MIC still resolves through its **own** zone name, and a MIC or
//! zone not listed here yields `None` rather than a guess.

use crate::asset::mic;
use chrono::{DateTime, Duration, NaiveDate, Utc};

/// `(MIC, IANA time zone)` for every venue whose local time we can compute.
pub const EXCHANGE_TIMEZONES: &[(&str, &str)] = &[
    (mic::FNSE, "Europe/Stockholm"),
    (mic::XSTO, "Europe/Stockholm"),
    (mic::XCSE, "Europe/Copenhagen"),
    (mic::FNDK, "Europe/Copenhagen"),
];

/// IANA time zone of `exchange_mic`, if known.
pub fn timezone(exchange_mic: &str) -> Option<&'static str> {
    EXCHANGE_TIMEZONES
        .iter()
        .find(|(m, _)| *m == exchange_mic)
        .map(|(_, tz)| *tz)
}

/// Hours `tz` is ahead of UTC at `utc`, for the zones we implement.
fn offset_hours(tz: &str, utc: DateTime<Utc>) -> Option<i64> {
    match tz {
        // Both follow the EU summer-time rule.
        "Europe/Stockholm" | "Europe/Copenhagen" => {
            Some(crate::stockholm::stockholm_offset_hours(utc))
        }
        _ => None,
    }
}

/// The calendar date at `exchange_mic`'s own exchange at the instant `utc`.
/// `None` for an unknown MIC.
pub fn local_date(exchange_mic: &str, utc: DateTime<Utc>) -> Option<NaiveDate> {
    let off = offset_hours(timezone(exchange_mic)?, utc)?;
    Some((utc + Duration::hours(off)).date_naive())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn every_listed_zone_has_an_offset_rule() {
        let now = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        for (m, _) in EXCHANGE_TIMEZONES {
            assert!(local_date(m, now).is_some(), "{m}");
        }
    }

    #[test]
    fn copenhagen_date_is_not_the_utc_date() {
        let winter = Utc.with_ymd_and_hms(2026, 1, 15, 23, 0, 0).unwrap();
        let summer = Utc.with_ymd_and_hms(2026, 7, 15, 22, 0, 0).unwrap();
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        assert_eq!(local_date("XCSE", winter), Some(d(2026, 1, 16)));
        assert_eq!(local_date("FNDK", summer), Some(d(2026, 7, 16)));
        assert_eq!(
            local_date("XCSE", winter - Duration::seconds(1)),
            Some(d(2026, 1, 15))
        );
    }

    #[test]
    fn unknown_mic_has_no_local_date() {
        assert_eq!(timezone("XNAS"), None);
        assert_eq!(local_date("XNAS", Utc::now()), None);
    }

    /// The `exchanges` seed must name the same zones as this table.
    #[test]
    fn timezones_agree_with_the_exchanges_seed() {
        let sql = include_str!("../../signal/migrations/20261027000000_create_exchanges.sql");
        let mut seen = 0;
        for line in sql.lines() {
            let parts: Vec<&str> = line
                .trim()
                .trim_start_matches('(')
                .split(',')
                .map(|p| p.trim().trim_matches('\''))
                .collect();
            if parts.len() > 3 && parts[2].starts_with("Europe/") {
                assert_eq!(timezone(parts[0]), Some(parts[2]), "{}", parts[0]);
                seen += 1;
            }
        }
        assert_eq!(seen, EXCHANGE_TIMEZONES.len());
    }
}
