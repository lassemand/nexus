//! Europe/Stockholm wall-clock helpers without a time-zone database.
//!
//! Stockholm follows the EU rule: CET (UTC+1) normally, CEST (UTC+2) from
//! 01:00 UTC on the last Sunday of March to 01:00 UTC on the last Sunday of
//! October. That rule has been stable since 1996.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Utc, Weekday};

fn last_sunday(year: i32, month: u32) -> NaiveDate {
    // Day 1 of the next month, stepped back to the last Sunday of `month`.
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let mut d = NaiveDate::from_ymd_opt(ny, nm, 1)
        .expect("valid first of month")
        .pred_opt()
        .expect("valid last of month");
    while d.weekday() != Weekday::Sun {
        d = d.pred_opt().expect("valid date");
    }
    d
}

/// Hours Stockholm is ahead of UTC at the instant `utc` (1 or 2).
pub fn stockholm_offset_hours(utc: DateTime<Utc>) -> i64 {
    let year = utc.year();
    let start = last_sunday(year, 3)
        .and_hms_opt(1, 0, 0)
        .expect("valid time")
        .and_utc();
    let end = last_sunday(year, 10)
        .and_hms_opt(1, 0, 0)
        .expect("valid time")
        .and_utc();
    if utc >= start && utc < end {
        2
    } else {
        1
    }
}

/// The Europe/Stockholm calendar date at the instant `utc`.
pub fn stockholm_date(utc: DateTime<Utc>) -> NaiveDate {
    (utc + Duration::hours(stockholm_offset_hours(utc))).date_naive()
}

/// The first instant after `after` at which Stockholm's wall clock reads
/// `hour:minute`.
pub fn next_stockholm_time(after: DateTime<Utc>, hour: u32, minute: u32) -> DateTime<Utc> {
    let first_day = stockholm_date(after);
    for day in 0..=2 {
        let local: NaiveDateTime = (first_day + Duration::days(day))
            .and_hms_opt(hour, minute, 0)
            .expect("valid wall-clock time");
        for offset in [2, 1] {
            let candidate = (local - Duration::hours(offset)).and_utc();
            if stockholm_offset_hours(candidate) == offset && candidate > after {
                return candidate;
            }
        }
    }
    // Unreachable for valid inputs: one of the next three days always matches.
    after + Duration::days(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, m: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, mi, 0).unwrap()
    }

    #[test]
    fn dst_boundaries_2026() {
        // 2026: starts Sunday 29 March 01:00Z, ends Sunday 25 October 01:00Z.
        assert_eq!(stockholm_offset_hours(utc(2026, 3, 29, 0, 59)), 1);
        assert_eq!(stockholm_offset_hours(utc(2026, 3, 29, 1, 0)), 2);
        assert_eq!(stockholm_offset_hours(utc(2026, 10, 25, 0, 59)), 2);
        assert_eq!(stockholm_offset_hours(utc(2026, 10, 25, 1, 0)), 1);
        assert_eq!(stockholm_offset_hours(utc(2026, 1, 15, 12, 0)), 1);
        assert_eq!(stockholm_offset_hours(utc(2026, 7, 15, 12, 0)), 2);
    }

    #[test]
    fn stockholm_date_is_not_the_utc_date() {
        // Winter: 23:00Z is already midnight (next day) in Stockholm.
        assert_eq!(
            stockholm_date(utc(2026, 1, 15, 23, 0)),
            "2026-01-16".parse().unwrap()
        );
        assert_eq!(
            stockholm_date(utc(2026, 1, 15, 22, 59)),
            "2026-01-15".parse().unwrap()
        );
        // Summer: 22:00Z is midnight (next day).
        assert_eq!(
            stockholm_date(utc(2026, 7, 15, 22, 0)),
            "2026-07-16".parse().unwrap()
        );
        assert_eq!(
            stockholm_date(utc(2026, 7, 15, 21, 59)),
            "2026-07-15".parse().unwrap()
        );
        // A bar stamped at UTC midnight is the same date in both seasons.
        assert_eq!(
            stockholm_date(utc(2026, 1, 15, 0, 0)),
            "2026-01-15".parse().unwrap()
        );
        assert_eq!(
            stockholm_date(utc(2026, 7, 15, 0, 0)),
            "2026-07-15".parse().unwrap()
        );
    }

    #[test]
    fn next_stockholm_time_handles_both_seasons() {
        // 18:30 local: 17:30Z in winter, 16:30Z in summer.
        assert_eq!(
            next_stockholm_time(utc(2026, 1, 15, 10, 0), 18, 30),
            utc(2026, 1, 15, 17, 30)
        );
        assert_eq!(
            next_stockholm_time(utc(2026, 1, 15, 18, 0), 18, 30),
            utc(2026, 1, 16, 17, 30)
        );
        assert_eq!(
            next_stockholm_time(utc(2026, 7, 15, 10, 0), 18, 30),
            utc(2026, 7, 15, 16, 30)
        );
        // Across the October change: Sat 24th 16:30Z (CEST) is followed by Sun 25th 17:30Z (CET).
        assert_eq!(
            next_stockholm_time(utc(2026, 10, 24, 17, 0), 18, 30),
            utc(2026, 10, 25, 17, 30)
        );
    }
}
