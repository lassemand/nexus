//! Danish: Danish issuers' releases.

use super::ReportLanguage;
use model::nordic::Lang;

/// Danish headline vocabulary. Danish issuers write no month ranges in titles,
/// so there are no month names here.
pub struct Danish;

impl ReportLanguage for Danish {
    fn lang(&self) -> Lang {
        Lang::Da
    }

    /// Half-year / annual / quarterly report, results announcement
    /// (`årsregnskabsmeddelelse` contains `regnskabsmeddelelse`).
    fn report_words(&self) -> &'static [&'static str] {
        &[
            "halvårsrapport",
            "årsrapport",
            "regnskabsmeddelelse",
            "kvartalsrapport",
        ]
    }

    fn hard_non_report_words(&self) -> &'static [&'static str] {
        &[
            "ledende medarbejdere",
            "aktietilbagekøb",
            "tilbagekøbsprogram",
            "storaktionær",
            "stemmerettigheder",
        ]
    }

    fn calendar_phrases(&self) -> &'static [&'static str] {
        &["finansiel kalender", "ændret dato"]
    }

    fn correction_markers(&self) -> &'static [&'static str] {
        &["rettelse:", "korrektion:"]
    }

    fn early_period_words(&self) -> &'static [&'static str] {
        &["1. kvartal", "2. kvartal", "3. kvartal"]
    }

    fn year_end_words(&self) -> &'static [&'static str] {
        &["årsregnskabsmeddelelse"]
    }

    /// Not inside `delårsrapport` / `halvårsrapport`.
    fn year_end_word_starts(&self) -> &'static [&'static str] {
        &["årsrapport"]
    }

    fn document_words(&self) -> &'static [&'static str] {
        &["årsrapport"]
    }

    fn results_words(&self) -> &'static [&'static str] {
        &["regnskab"]
    }

    fn quarter_phrases(&self) -> &'static [(&'static str, u8)] {
        &[
            ("1. kvartal", 1),
            ("2. kvartal", 2),
            ("3. kvartal", 3),
            ("4. kvartal", 4),
        ]
    }
}
