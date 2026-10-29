//! Denmark: Nasdaq Copenhagen (`XCSE`) and Nasdaq First North Denmark (`FNDK`).
//!
//! Sources: session hours as in the signal `exchanges` seed (Nasdaq's European
//! trading-hours page); holiday rules from the Nager.Date `DK` feed plus the
//! closures below; Saxo ids follow Saxo's naming. Carried over unchanged from
//! the former per-crate tables on 2026-10-29 (NEX-211); not re-verified then.

use super::{Country, ExtraClosure, HolidayRules, InsiderSourceKind, Lang, LocalTime, Venue};
use crate::asset::mic;

const OPEN: LocalTime = LocalTime::new(9, 0);
const CLOSE: LocalTime = LocalTime::new(17, 0);

/// Denmark.
pub const DENMARK: Country = Country {
    code: "DK",
    name: "Denmark",
    currency: "DKK",
    timezone: "Europe/Copenhagen",
    venues: &[
        Venue {
            mic: mic::XCSE,
            name: "Nasdaq Copenhagen",
            yahoo_exchange_codes: &["CPH"],
            saxo_exchange_id: "CSE",
            saxo_verified: false,
            open_local: OPEN,
            close_local: CLOSE,
            half_day_close_local: None,
        },
        Venue {
            mic: mic::FNDK,
            name: "Nasdaq First North Denmark",
            yahoo_exchange_codes: &[],
            saxo_exchange_id: "CSE_FN-DK",
            saxo_verified: false,
            open_local: OPEN,
            close_local: CLOSE,
            half_day_close_local: None,
        },
    ],
    // 09:00-17:00, no half days. Nager.Date lists the bank holidays too, but
    // the extra closures make the result independent of how it types them.
    holidays: HolidayRules {
        nager_skip: &[
            "Easter Sunday", // always a Sunday
            "Pentecost",     // always a Sunday
        ],
        nager_half_days: &[],
        extra_closures: &[
            ExtraClosure::EasterOffset {
                days: 40,
                note: "Friday after Ascension Day",
                last_year: None,
            },
            ExtraClosure::Fixed {
                month: 6,
                day: 5,
                note: "Constitution Day",
            },
            ExtraClosure::Fixed {
                month: 12,
                day: 24,
                note: "Christmas Eve",
            },
            ExtraClosure::Fixed {
                month: 12,
                day: 31,
                note: "New Year's Eve",
            },
            // Store Bededag, abolished from 2024.
            ExtraClosure::EasterOffset {
                days: 26,
                note: "Great Prayer Day",
                last_year: Some(2023),
            },
        ],
    },
    insider_source: InsiderSourceKind::None,
    languages: &[Lang::Da, Lang::En],
};
