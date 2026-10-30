//! Finland: Nasdaq Helsinki (`XHEL`) and Nasdaq First North Finland (`FNFI`).
//!
//! Sources: session hours (10:00-18:30, the end of the closing auction, no
//! half days) as in the signal `exchanges` seed (Nasdaq's European
//! trading-hours page); holiday rules computed below because Nager.Date's FI
//! list has Saturday-only holidays and Sundays and lacks 24 and 31 December;
//! Saxo ids are unverified guesses from Saxo's naming. Prices are in EUR.
//! Carried over unchanged from the former per-crate tables on 2026-10-29
//! (NEX-211); not re-verified then.

use super::{Country, ExtraClosure, HolidayRules, InsiderSourceKind, Lang, LocalTime, Venue};
use crate::asset::mic;

const OPEN: LocalTime = LocalTime::new(10, 0);
const CLOSE: LocalTime = LocalTime::new(18, 30);

/// Finland.
pub const FINLAND: Country = Country {
    code: "FI",
    name: "Finland",
    currency: "EUR",
    timezone: "Europe/Helsinki",
    venues: &[
        Venue {
            mic: mic::XHEL,
            name: "Nasdaq Helsinki",
            yahoo_exchange_codes: &["HEL"],
            saxo_exchange_id: "HSE",
            saxo_verified: false,
            open_local: OPEN,
            close_local: CLOSE,
            half_day_close_local: None,
        },
        Venue {
            mic: mic::FNFI,
            name: "Nasdaq First North Finland",
            yahoo_exchange_codes: &[],
            saxo_exchange_id: "HSE_FN-FI",
            saxo_verified: false,
            open_local: OPEN,
            close_local: CLOSE,
            half_day_close_local: None,
        },
    ],
    // The full closure set is computed, so the result does not depend on how
    // Nager names or types a date (a date it already lists is kept once).
    holidays: HolidayRules {
        nager_skip: &[
            "Easter Sunday",   // always a Sunday
            "Pentecost",       // always a Sunday
            "Whit Sunday",     // alternate name for Pentecost
            "Midsummer Day",   // the Saturday; Midsummer Eve is the closure
            "All Saints' Day", // a Saturday, not an exchange closure
        ],
        nager_half_days: &[],
        extra_closures: &[
            ExtraClosure::Fixed {
                month: 1,
                day: 1,
                note: "New Year's Day",
            },
            ExtraClosure::Fixed {
                month: 1,
                day: 6,
                note: "Epiphany",
            },
            ExtraClosure::EasterOffset {
                days: -2,
                note: "Good Friday",
                last_year: None,
            },
            ExtraClosure::EasterOffset {
                days: 1,
                note: "Easter Monday",
                last_year: None,
            },
            ExtraClosure::Fixed {
                month: 5,
                day: 1,
                note: "May Day",
            },
            ExtraClosure::EasterOffset {
                days: 39,
                note: "Ascension Day",
                last_year: None,
            },
            ExtraClosure::MidsummerEve {
                note: "Midsummer Eve",
            },
            // Weekday-only: the calendar never stores a weekend date.
            ExtraClosure::Fixed {
                month: 12,
                day: 6,
                note: "Independence Day",
            },
            ExtraClosure::Fixed {
                month: 12,
                day: 24,
                note: "Christmas Eve",
            },
            ExtraClosure::Fixed {
                month: 12,
                day: 25,
                note: "Christmas Day",
            },
            ExtraClosure::Fixed {
                month: 12,
                day: 26,
                note: "Boxing Day",
            },
            ExtraClosure::Fixed {
                month: 12,
                day: 31,
                note: "New Year's Eve",
            },
        ],
    },
    insider_source: InsiderSourceKind::MarReleases,
    languages: &[Lang::Fi, Lang::Sv, Lang::En],
};
