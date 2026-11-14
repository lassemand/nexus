//! When a simulated trade enters, exits and is observed: minute-precision
//! instants from the exchange calendar ([`model::session::Sessions`]).
//!
//! Continuous trading on a day is `[open, close - CLOSE_BUFFER)` in the
//! exchange's own time zone, the half-day close on a half day. A weekend or a
//! holiday has no session. A date the calendar does not cover, or a MIC it does
//! not know, is [`EntryPlan::CalendarUnknown`]: never guessed.

use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, Utc};
use model::session::{Day, Sessions};

use crate::config::Horizon;

/// How many days ahead the next session is looked for before giving up.
const LOOK_AHEAD_DAYS: i64 = 14;

/// Where an entry quote is first asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryPlan {
    /// The decision is inside continuous trading: ask at once.
    Now,
    /// Ask at the next session open, which is at this instant.
    At(DateTime<Utc>),
    /// The calendar does not say: no quotes, status `calendar_unknown`.
    CalendarUnknown,
}

impl EntryPlan {
    /// The instant of the first entry quote request given the decision time.
    pub fn target(self, decided_at: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            EntryPlan::Now => Some(decided_at),
            EntryPlan::At(at) => Some(at),
            EntryPlan::CalendarUnknown => None,
        }
    }
}

fn chrono_duration(d: Duration) -> ChronoDuration {
    ChronoDuration::from_std(d).unwrap_or(ChronoDuration::MAX)
}

/// Where the entry of a decision taken at `decided_at` on `mic` is first asked for.
pub fn plan_entry(
    sessions: &Sessions,
    mic: &str,
    decided_at: DateTime<Utc>,
    close_buffer: Duration,
) -> EntryPlan {
    let Some(date) = sessions.local_date(mic, decided_at) else {
        return EntryPlan::CalendarUnknown;
    };
    let buffer = chrono_duration(close_buffer);
    match sessions.day(mic, date) {
        Day::Unknown => return EntryPlan::CalendarUnknown,
        Day::Trading(window) => {
            if decided_at < window.open {
                return EntryPlan::At(window.open);
            }
            if decided_at < window.close - buffer {
                return EntryPlan::Now;
            }
        }
        Day::Closed => {}
    }
    next_open(sessions, mic, date)
}

fn next_open(sessions: &Sessions, mic: &str, after: NaiveDate) -> EntryPlan {
    for offset in 1..=LOOK_AHEAD_DAYS {
        let Some(date) = after.checked_add_signed(ChronoDuration::days(offset)) else {
            break;
        };
        match sessions.day(mic, date) {
            Day::Unknown => return EntryPlan::CalendarUnknown,
            Day::Closed => {}
            Day::Trading(window) => return EntryPlan::At(window.open),
        }
    }
    EntryPlan::CalendarUnknown
}

/// The end of continuous trading on the local day of `at`: the close minus the
/// buffer. `None` on a day without a session or an unknown calendar.
pub fn session_cutoff(
    sessions: &Sessions,
    mic: &str,
    at: DateTime<Utc>,
    close_buffer: Duration,
) -> Option<DateTime<Utc>> {
    let date = sessions.local_date(mic, at)?;
    match sessions.day(mic, date) {
        Day::Trading(window) => Some(window.close - chrono_duration(close_buffer)),
        Day::Unknown | Day::Closed => None,
    }
}

/// The instant the position is closed: `HOLD` after entry, but never past the
/// cutoff. `None` when the entry is not before the cutoff (`invalid_entry`).
pub fn exit_target(
    entry_at: DateTime<Utc>,
    hold: Duration,
    cutoff: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    (entry_at < cutoff).then(|| (entry_at + chrono_duration(hold)).min(cutoff))
}

/// When one observation is taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    /// Take the bid at this instant.
    At(DateTime<Utc>),
    /// The horizon is past the cutoff: recorded as `beyond_close`.
    BeyondClose,
}

/// The slot of every horizon after an entry at `entry_at`, with its label. A
/// horizon of `close` is the cutoff; one that lands after the cutoff is
/// `beyond_close`.
pub fn observation_slots(
    entry_at: DateTime<Utc>,
    cutoff: DateTime<Utc>,
    horizons: &[Horizon],
) -> Vec<(String, Slot)> {
    horizons
        .iter()
        .map(|horizon| {
            let slot = match horizon {
                Horizon::Close => Slot::At(cutoff),
                Horizon::After(d) => {
                    let at = entry_at + chrono_duration(*d);
                    if at > cutoff {
                        Slot::BeyondClose
                    } else {
                        Slot::At(at)
                    }
                }
            };
            (horizon.label(), slot)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveTime, TimeZone};
    use chrono_tz::Tz;
    use model::session::{Calendar, Exchange};

    use super::*;

    const BUFFER: Duration = Duration::from_secs(5 * 60);

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).expect("date")
    }

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).expect("time")
    }

    fn market(mic: &str, tz: Tz, open: NaiveTime, close: NaiveTime) -> (Exchange, Calendar) {
        let closed = [d(2026, 1, 1), d(2026, 4, 3), d(2026, 4, 6), d(2026, 12, 25)];
        (
            Exchange {
                mic: mic.into(),
                country: "XX".into(),
                timezone: tz,
                open,
                close,
                half_day_close: Some(t(13, 0)),
            },
            Calendar::from_holidays(
                closed.into_iter().collect(),
                [d(2026, 12, 24)].into_iter().collect(),
            )
            .expect("calendar"),
        )
    }

    fn sessions() -> Sessions {
        Sessions::new(vec![
            market("XSTO", chrono_tz::Europe::Stockholm, t(9, 0), t(17, 30)),
            market("XHEL", chrono_tz::Europe::Helsinki, t(10, 0), t(18, 30)),
        ])
    }

    fn sto(m: u32, day: u32, h: u32, mi: u32) -> DateTime<Utc> {
        chrono_tz::Europe::Stockholm
            .with_ymd_and_hms(2026, m, day, h, mi, 0)
            .single()
            .expect("instant")
            .with_timezone(&Utc)
    }

    #[test]
    fn a_pre_open_release_enters_at_the_open() {
        let plan = plan_entry(&sessions(), "XSTO", sto(11, 10, 7, 30), BUFFER);
        assert_eq!(plan, EntryPlan::At(sto(11, 10, 9, 0)));
    }

    #[test]
    fn a_release_during_trading_enters_at_once() {
        let at = sto(11, 10, 11, 0);
        assert_eq!(plan_entry(&sessions(), "XSTO", at, BUFFER), EntryPlan::Now);
        let late = sto(11, 10, 17, 10);
        assert_eq!(
            plan_entry(&sessions(), "XSTO", late, BUFFER),
            EntryPlan::Now
        );
    }

    #[test]
    fn a_release_after_the_cutoff_enters_the_next_trading_day() {
        let sessions = sessions();
        assert_eq!(
            plan_entry(&sessions, "XSTO", sto(11, 10, 18, 0), BUFFER),
            EntryPlan::At(sto(11, 11, 9, 0))
        );
        assert_eq!(
            plan_entry(&sessions, "XSTO", sto(11, 10, 17, 25), BUFFER),
            EntryPlan::At(sto(11, 11, 9, 0)),
            "inside the closing auction buffer"
        );
        assert_eq!(
            plan_entry(&sessions, "XSTO", sto(11, 12, 18, 0), BUFFER),
            EntryPlan::At(sto(11, 13, 9, 0)),
            "a Thursday evening"
        );
    }

    #[test]
    fn a_weekend_and_an_easter_monday_are_skipped() {
        let sessions = sessions();
        assert_eq!(
            plan_entry(&sessions, "XSTO", sto(4, 2, 18, 0), BUFFER),
            EntryPlan::At(sto(4, 7, 9, 0)),
            "Thursday before Good Friday, Easter Monday: Tuesday"
        );
        assert_eq!(
            plan_entry(&sessions, "XSTO", sto(11, 14, 12, 0), BUFFER),
            EntryPlan::At(sto(11, 16, 9, 0)),
            "a Saturday"
        );
    }

    #[test]
    fn a_helsinki_company_follows_helsinki_hours() {
        let at = chrono_tz::Europe::Helsinki
            .with_ymd_and_hms(2026, 11, 10, 9, 30, 0)
            .single()
            .expect("instant")
            .with_timezone(&Utc);
        let open = chrono_tz::Europe::Helsinki
            .with_ymd_and_hms(2026, 11, 10, 10, 0, 0)
            .single()
            .expect("instant")
            .with_timezone(&Utc);
        assert_eq!(
            plan_entry(&sessions(), "XHEL", at, BUFFER),
            EntryPlan::At(open)
        );
    }

    #[test]
    fn a_half_day_cuts_the_session_at_12_55() {
        let sessions = sessions();
        assert_eq!(
            session_cutoff(&sessions, "XSTO", sto(12, 24, 10, 0), BUFFER),
            Some(sto(12, 24, 12, 55))
        );
        assert_eq!(
            session_cutoff(&sessions, "XSTO", sto(11, 10, 10, 0), BUFFER),
            Some(sto(11, 10, 17, 25))
        );
        assert_eq!(
            session_cutoff(&sessions, "XSTO", sto(11, 14, 10, 0), BUFFER),
            None
        );
    }

    #[test]
    fn unknown_calendars_are_never_guessed() {
        let sessions = sessions();
        assert_eq!(
            plan_entry(&sessions, "XOSL", sto(11, 10, 11, 0), BUFFER),
            EntryPlan::CalendarUnknown
        );
        assert_eq!(
            plan_entry(
                &sessions,
                "XSTO",
                sto(11, 10, 11, 0) + ChronoDuration::days(400),
                BUFFER
            ),
            EntryPlan::CalendarUnknown,
            "a year the table does not cover"
        );
        assert_eq!(
            plan_entry(&sessions, "XSTO", sto(12, 31, 18, 0), BUFFER),
            EntryPlan::CalendarUnknown,
            "the next session is in a year the table does not cover"
        );
        assert_eq!(EntryPlan::CalendarUnknown.target(sto(11, 10, 11, 0)), None);
    }

    #[test]
    fn the_exit_is_hold_after_entry_but_never_past_the_cutoff() {
        let hold = Duration::from_secs(30 * 60);
        let cutoff = sto(11, 10, 17, 25);
        assert_eq!(
            exit_target(sto(11, 10, 11, 0), hold, cutoff),
            Some(sto(11, 10, 11, 30))
        );
        assert_eq!(
            exit_target(sto(11, 10, 17, 10), hold, cutoff),
            Some(cutoff),
            "17:10 + 30 min is after the cutoff, not 17:40"
        );
        assert_eq!(exit_target(cutoff, hold, cutoff), None);
        assert_eq!(exit_target(sto(11, 10, 17, 30), hold, cutoff), None);
    }

    #[test]
    fn observations_past_the_cutoff_are_beyond_close_and_close_is_the_cutoff() {
        let horizons = crate::config::parse_horizons("1m,5m,15m,30m,60m,close").expect("valid");
        let cutoff = sto(11, 10, 17, 25);
        let slots = observation_slots(sto(11, 10, 17, 0), cutoff, &horizons);
        let labels: Vec<_> = slots.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["1m", "5m", "15m", "30m", "60m", "close"]);
        assert_eq!(slots[0].1, Slot::At(sto(11, 10, 17, 1)));
        assert_eq!(slots[2].1, Slot::At(sto(11, 10, 17, 15)));
        assert_eq!(slots[3].1, Slot::BeyondClose);
        assert_eq!(slots[4].1, Slot::BeyondClose);
        assert_eq!(slots[5].1, Slot::At(cutoff));
        let exactly = observation_slots(sto(11, 10, 17, 10), cutoff, &horizons);
        assert_eq!(
            exactly[2].1,
            Slot::At(cutoff),
            "landing on the cutoff is allowed"
        );
    }
}
