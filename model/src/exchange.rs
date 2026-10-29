//! Exchange-local time for the Nordic venues, without a time-zone database.
//!
//! The IANA zone names come from [`crate::nordic`] and equal
//! `exchanges.timezone` in the signal database (a test in `signal` keeps the two
//! in step). Stockholm and
//! Copenhagen both follow the EU rule (CET/CEST), so one offset function serves
//! both; each MIC still resolves through its **own** zone name, and a MIC or
//! zone not listed here yields `None` rather than a guess.

use chrono::{DateTime, Duration, NaiveDate, Utc};

/// IANA time zone of `exchange_mic`, if known (from [`crate::nordic`]).
pub fn timezone(exchange_mic: &str) -> Option<&'static str> {
    crate::nordic::country_for_mic(exchange_mic).map(|c| c.timezone)
}

/// Hours `tz` is ahead of UTC at `utc`, for the zones we implement.
fn offset_hours(tz: &str, utc: DateTime<Utc>) -> Option<i64> {
    match tz {
        // Both follow the EU summer-time rule.
        "Europe/Stockholm" | "Europe/Copenhagen" => {
            Some(crate::stockholm::stockholm_offset_hours(utc))
        }
        // Helsinki has its own zone (one hour ahead of Stockholm); it comes from
        // the tz database, never as an offset from Stockholm.
        "Europe/Helsinki" => {
            use chrono::{Offset, TimeZone};
            let off = chrono_tz::Europe::Helsinki.offset_from_utc_datetime(&utc.naive_utc());
            Some(i64::from(off.fix().local_minus_utc()) / 3600)
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
        for m in crate::nordic::nordic_mics() {
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
    fn helsinki_is_one_hour_ahead_of_stockholm_and_follows_dst() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        // 22:30 UTC: already the next day in Helsinki, not yet in Stockholm.
        let winter = Utc.with_ymd_and_hms(2026, 1, 15, 22, 30, 0).unwrap();
        assert_eq!(local_date("XHEL", winter), Some(d(2026, 1, 16)));
        assert_eq!(local_date("XSTO", winter), Some(d(2026, 1, 15)));
        // EEST (UTC+3) from the last Sunday of March, 01:00 UTC.
        let before = Utc.with_ymd_and_hms(2026, 3, 29, 0, 59, 0).unwrap();
        let after = Utc.with_ymd_and_hms(2026, 3, 29, 1, 0, 0).unwrap();
        assert_eq!(offset_hours("Europe/Helsinki", before), Some(2));
        assert_eq!(offset_hours("Europe/Helsinki", after), Some(3));
        let summer = Utc.with_ymd_and_hms(2026, 7, 15, 21, 0, 0).unwrap();
        assert_eq!(local_date("FNFI", summer), Some(d(2026, 7, 16)));
    }

    #[test]
    fn unknown_mic_has_no_local_date() {
        assert_eq!(timezone("XNAS"), None);
        assert_eq!(local_date("XNAS", Utc::now()), None);
    }
}
