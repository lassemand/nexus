//! Sweden: Nasdaq Stockholm (`XSTO`) and Nasdaq First North Growth Market
//! Stockholm (`FNSE`).
//!
//! Sources: session hours as in the signal `exchanges` seed (Nasdaq's European
//! trading-hours page); holiday rules from the Nager.Date `SE` feed plus the
//! rules below; Saxo `SSE_FN-SE` seen on a live First North instrument, the
//! other id follows Saxo's naming. Carried over unchanged from the former
//! per-crate tables on 2026-10-29 (NEX-211); not re-verified then.

use super::{Country, HolidayRules, InsiderSourceKind, Lang, LocalTime, Venue};
use crate::asset::mic;

const OPEN: LocalTime = LocalTime::new(9, 0);
const CLOSE: LocalTime = LocalTime::new(17, 30);
const HALF_DAY_CLOSE: LocalTime = LocalTime::new(13, 0);

/// Sweden.
pub const SWEDEN: Country = Country {
    code: "SE",
    name: "Sweden",
    currency: "SEK",
    timezone: "Europe/Stockholm",
    venues: &[
        Venue {
            mic: mic::FNSE,
            name: "Nasdaq First North Growth Market Stockholm",
            yahoo_exchange_codes: &["STO"],
            saxo_exchange_id: "SSE_FN-SE",
            saxo_verified: true,
            open_local: OPEN,
            close_local: CLOSE,
            half_day_close_local: Some(HALF_DAY_CLOSE),
        },
        Venue {
            mic: mic::XSTO,
            name: "Nasdaq Stockholm",
            yahoo_exchange_codes: &[],
            saxo_exchange_id: "SSE",
            saxo_verified: false,
            open_local: OPEN,
            close_local: CLOSE,
            half_day_close_local: Some(HALF_DAY_CLOSE),
        },
    ],
    // FIXME(swedish-holidays, no bug id yet): published Stockholm calendars list Jan 6, Dec 24 and
    // Dec 31 as full closures, with 13:00 half days on Jan 5, Maundy Thursday,
    // Apr 30, the day before Ascension and the Friday before All Saints'.
    // These rules skip Epiphany and treat Dec 24 / Dec 31 as half days. Kept
    // as they were; fixing it is its own task.
    holidays: HolidayRules {
        nager_skip: &[
            "Epiphany",        // Jan 6 — not observed by Nasdaq Stockholm
            "Easter Sunday",   // always a Sunday
            "Pentecost",       // always a Sunday
            "Whit Sunday",     // alternate name for Pentecost
            "Midsummer Day",   // the Saturday — Midsummer Eve (Friday) is the closure
            "All Saints' Day", // Nov 1 — not observed
        ],
        nager_half_days: &[
            "Christmas Eve",  // Dec 24 — early close 13:00 CET
            "New Year's Eve", // Dec 31 — early close 13:00 CET
        ],
        extra_closures: &[],
    },
    insider_source: InsiderSourceKind::FiRegister,
    languages: &[Lang::Sv, Lang::En],
};
