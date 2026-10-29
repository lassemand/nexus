//! Finnish: Finnish issuers' releases (their Swedish and English copies use
//! [`super::sv`] and [`super::en`]).

use super::{range_period, RangePeriod, ReportLanguage};
use model::nordic::Lang;

/// Finnish headline vocabulary.
pub struct Finnish;

/// Finnish month stems; every case form starts with one
/// (`tammikuuta`, `tammikuun`, `tammikuulta`).
const MONTHS: [&str; 12] = [
    "tammikuu",
    "helmikuu",
    "maaliskuu",
    "huhtikuu",
    "toukokuu",
    "kesäkuu",
    "heinäkuu",
    "elokuu",
    "syyskuu",
    "lokakuu",
    "marraskuu",
    "joulukuu",
];

/// The short forms used before a hyphen: `tammi-kesäkuulta`.
const MONTHS_SHORT: [&str; 12] = [
    "tammi", "helmi", "maalis", "huhti", "touko", "kesä", "heinä", "elo", "syys", "loka", "marras",
    "joulu",
];

impl ReportLanguage for Finnish {
    fn lang(&self) -> Lang {
        Lang::Fi
    }

    /// Interim, half-year, financial statements release (the Q4 / full-year
    /// report), business review. The genitive forms (`osavuosikatsauksen`) are
    /// not here on purpose: they occur in calendar notices ("julkaisee ...
    /// osavuosikatsauksen").
    fn report_words(&self) -> &'static [&'static str] {
        &[
            "osavuosikatsaus",
            "puolivuosikatsaus",
            "tilinpäätöstiedote",
            "liiketoimintakatsaus",
        ]
    }

    /// Managers' transactions, share buy-backs, major-shareholder (flagging)
    /// notices and the legal formula that opens them. Whole phrases:
    /// `liiketoimet` is also a stem of `liiketoimintakatsaus` (business
    /// review), which IS a report.
    fn hard_non_report_words(&self) -> &'static [&'static str] {
        &[
            "johtohenkilöiden liiketoimet",
            "johdon liiketoimet",
            "omien osakkeiden hankinta",
            "omien osakkeiden ostot",
            "liputusilmoitus",
            "arvopaperimarkkinalain 9 luvun",
        ]
    }

    fn news_words(&self) -> &'static [&'static str] {
        &["tulosvaroitus", "tulosnäkym"]
    }

    /// Annual report and the general meeting (`yhtiökokouksen` has no
    /// `yhtiökokous`) and notices.
    fn non_report_words(&self) -> &'static [&'static str] {
        &["vuosikertomus", "yhtiökokou", "kutsu", "oikaist"]
    }

    /// "Neles julkaisee tammi-joulukuun 2021 tilinpäätöstiedotteen 4.
    /// helmikuuta": announces when a report will come out. The accusative forms
    /// (`osavuosikatsauksen`) differ from the nominative the report titles use.
    fn is_calendar_notice(&self, lower: &str) -> bool {
        const ANNOUNCES: &[&str] = &["julkaisee", "julkistaa", "julkaiseminen"];
        // Stems of the accusative / possessive forms (`osavuosikatsauksen`,
        // `osavuosikatsauksensa`); the nominative `osavuosikatsaus` of a report
        // title does not contain them.
        const REPORTS: &[&str] = &[
            "osavuosikatsauks",
            "puolivuosikatsauks",
            "osavuotiskatsauks",
            "puolivuotiskatsauks",
            "tilinpäätöstiedotte",
            "liiketoimintakatsauks",
        ];
        ANNOUNCES.iter().any(|a| lower.contains(a)) && REPORTS.iter().any(|r| lower.contains(r))
    }

    fn correction_markers(&self) -> &'static [&'static str] {
        &["oikaisu:", "korjaus:"]
    }

    fn early_period_words(&self) -> &'static [&'static str] {
        &["puolivuosikatsaus"]
    }

    fn year_end_words(&self) -> &'static [&'static str] {
        &[
            "tilinpäätöstiedote",
            "viimeisen neljänneksen",
            "neljännen neljänneksen",
        ]
    }

    fn document_words(&self) -> &'static [&'static str] {
        &["vuosikertomus"]
    }

    fn quarter_phrases(&self) -> &'static [(&'static str, u8)] {
        &[
            ("ensimmäisen neljänneksen", 1),
            ("ensimmäinen neljännes", 1),
            ("toisen neljänneksen", 2),
            ("toinen neljännes", 2),
            ("kolmannen neljänneksen", 3),
            ("kolmas neljännes", 3),
            ("viimeisen neljänneksen", 4),
            ("neljännen neljänneksen", 4),
            ("puolivuosikatsaus", 2),
        ]
    }

    fn month_names(&self) -> &'static [&'static str] {
        &MONTHS
    }

    /// A stem match covers every case form; a short form only counts when a
    /// dash follows it, so `loka` or `kesä` elsewhere are not months.
    fn month_number(&self, word: &str, next: Option<char>) -> Option<u8> {
        let dash = matches!(next, Some('-' | '\u{2013}' | '\u{2014}'));
        (0..12).find_map(|i| {
            let hit = word.starts_with(MONTHS[i]) || (dash && word == MONTHS_SHORT[i]);
            hit.then_some(i as u8 + 1)
        })
    }

    /// The period of a numeric range such as `1.1. - 30.9.2013` or `1.1.-31.12.2011`.
    fn numeric_range(&self, lower: &str) -> Option<RangePeriod> {
        /// `d.m.` at the start of `s`: the month and the rest.
        fn day_month(s: &str) -> Option<(u8, &str)> {
            let s = s.trim_start();
            let day_end = s.find(|c: char| !c.is_ascii_digit())?;
            let (day, rest) = s.split_at(day_end);
            let rest = rest.strip_prefix('.')?;
            let month_end = rest.find(|c: char| !c.is_ascii_digit())?;
            let (month, rest) = rest.split_at(month_end);
            let rest = rest.strip_prefix('.')?;
            let (day, month): (u8, u8) = (day.parse().ok()?, month.parse().ok()?);
            ((1..=31).contains(&day) && (1..=12).contains(&month)).then_some((month, rest))
        }
        lower
            .char_indices()
            .filter(|(_, c)| c.is_ascii_digit())
            .find_map(|(i, _)| {
                if i > 0 && lower.as_bytes()[i - 1].is_ascii_digit() {
                    return None;
                }
                let (a, rest) = day_month(&lower[i..])?;
                let rest = rest
                    .trim_start()
                    .strip_prefix(['-', '\u{2013}', '\u{2014}'])?;
                let (b, _) = day_month(rest)?;
                range_period(a, b)
            })
    }
}
