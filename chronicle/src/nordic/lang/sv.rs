//! Swedish: Swedish issuers, Finnish issuers' Swedish copies and MFN's calendar.

use super::ReportLanguage;
use model::nordic::Lang;

/// Swedish headline vocabulary.
pub struct Swedish;

const MONTHS: [&str; 12] = [
    "januari",
    "februari",
    "mars",
    "april",
    "maj",
    "juni",
    "juli",
    "augusti",
    "september",
    "oktober",
    "november",
    "december",
];

impl ReportLanguage for Swedish {
    fn lang(&self) -> Lang {
        Lang::Sv
    }

    fn report_words(&self) -> &'static [&'static str] {
        &["rapport", "bokslutskommuniké", "delårsrapport"]
    }

    fn non_report_words(&self) -> &'static [&'static str] {
        &[
            "bolagsstämma",
            "årsstämma",
            "valberedning",
            "årsredovisning",
            "hållbarhets",
            "ersättnings",
            "bolagsstyrning",
            "inbjudan",
            "kallelse",
            "återköp",
        ]
    }

    fn quarter_words(&self) -> &'static [&'static str] {
        // Also the Danish and Norwegian word.
        &["kvartal"]
    }

    fn calendar_phrases(&self) -> &'static [&'static str] {
        // `finanskalender` is also the Danish spelling.
        &["finansiell kalender", "finanskalender", "ändrad rapportdag"]
    }

    fn calendar_notice_phrases(&self) -> &'static [&'static str] {
        &["tidigarelägger", "senarelägger"]
    }

    fn is_calendar_notice(&self, lower: &str) -> bool {
        lower.contains("flyttar") && lower.contains("rapport")
    }

    fn correction_markers(&self) -> &'static [&'static str] {
        &["rättelse:"]
    }

    fn early_period_words(&self) -> &'static [&'static str] {
        &[
            "första kvartalet",
            "andra kvartalet",
            "tredje kvartalet",
            "halvår",
        ]
    }

    fn year_end_words(&self) -> &'static [&'static str] {
        &["fjärde kvartalet", "bokslutskommuniké"]
    }

    fn full_year_words(&self) -> &'static [&'static str] {
        // Also the Danish word.
        &["helår"]
    }

    fn document_words(&self) -> &'static [&'static str] {
        &["årsredovisning"]
    }

    fn quarter_phrases(&self) -> &'static [(&'static str, u8)] {
        &[
            ("första kvartalet", 1),
            ("andra kvartalet", 2),
            ("tredje kvartalet", 3),
            ("fjärde kvartalet", 4),
        ]
    }

    fn month_names(&self) -> &'static [&'static str] {
        &MONTHS
    }
}
