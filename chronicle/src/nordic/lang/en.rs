//! English: the working language of many issuers on all three markets.

use super::{has_h1_token, ReportLanguage};
use model::nordic::Lang;

/// English headline vocabulary.
pub struct English;

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

impl ReportLanguage for English {
    fn lang(&self) -> Lang {
        Lang::En
    }

    fn report_words(&self) -> &'static [&'static str] {
        &[
            "report",
            "interim",
            "half-year",
            "year-end",
            "annual",
            "financial statement for",
            // Finnish issuers' English twins.
            "financial statements review",
            "business review",
        ]
    }

    fn hard_non_report_words(&self) -> &'static [&'static str] {
        &[
            "persons discharging managerial responsibilities",
            "managerial responsibilities",
            "share repurchase",
            "share buy-back",
            "major shareholder",
            "voting rights",
            "total number of voting rights and capital",
            // Managers' transactions (the English twins of the Finnish ones).
            "managers' transaction",
            "managers\u{2019} transaction",
            "share buyback",
        ]
    }

    fn news_words(&self) -> &'static [&'static str] {
        &[
            "order intake",
            "guidance",
            "contract",
            "agreement",
            "secures",
            "signs",
            "market update",
            // Profit warnings and outlook changes that name a quarter: not results.
            "outlook",
            "profit warning",
            "upgrades",
            "withdraws",
            "impact of",
        ]
    }

    fn non_report_words(&self) -> &'static [&'static str] {
        &[
            "general meeting",
            "nomination committee",
            "annual report",
            // Issuers' notices that merely contain "report"/"interim": a
            // schedule, restated or recast figures, a segment change, an interim
            // President, the annual review document.
            "reporting schedule",
            "financial reporting",
            "segment reporting",
            "reporting segment",
            "reporting structure",
            "restated",
            "recast",
            "interim president",
            "interim ceo",
            "interim cfo",
            "interim evp",
            "interim change",
            "annual review",
            "sustainability",
            "remuneration",
            "corporate governance",
            "invitation",
            "conference call",
            "telephone conference",
            "webcast",
            "notice of",
            "notice to",
            "repurchase",
            "reporting practices",
            "update on",
            "treasury",
        ]
    }

    /// Pharma headlines such as "Novo Nordisk phase 2 trial with amycretin
    /// reports significant weight loss" contain "reports" but are not results.
    fn clinical_words(&self) -> &'static [&'static str] {
        &[
            "trial", "phase 1", "phase 2", "phase 3", "clinical", "fda", "chmp",
        ]
    }

    fn quarter_words(&self) -> &'static [&'static str] {
        &["quarter"]
    }

    fn calendar_phrases(&self) -> &'static [&'static str] {
        &[
            "financial calendar",
            "changes date of",
            "rescheduled",
            "reporting date",
            "report date",
        ]
    }

    fn calendar_notice_phrases(&self) -> &'static [&'static str] {
        &[
            "early publication",
            "delayed publication",
            "postpone",
            "brings forward",
            "moves up publication",
            "new date for",
        ]
    }

    /// "Neles to publish its Interim Review ... on February 4, 2022",
    /// "Publishing of Outotec's Interim Report ...", "KONE publishes ... on
    /// Thursday, October 25". A date follows.
    fn is_calendar_notice(&self, lower: &str) -> bool {
        const ANNOUNCES: &[&str] = &["to publish", "publishing of", "publishes"];
        const REPORTS: &[&str] = &[
            "interim",
            "half-year",
            "half year",
            "financial statements",
            "year-end",
        ];
        REPORTS.iter().any(|r| lower.contains(r))
            && (lower.contains("publishing of")
                || (ANNOUNCES.iter().any(|a| lower.contains(a))
                    && lower.contains(" on ")
                    && MONTHS.iter().any(|m| lower.contains(m))))
    }

    fn correction_markers(&self) -> &'static [&'static str] {
        &["correction:"]
    }

    fn early_period_words(&self) -> &'static [&'static str] {
        &[
            "first quarter",
            "second quarter",
            "third quarter",
            "half-year",
            "first half",
            "first three months",
            "first six months",
            "first nine months",
        ]
    }

    fn year_end_words(&self) -> &'static [&'static str] {
        &[
            "fourth quarter",
            "year-end",
            "financial statement for",
            "financial statements review",
        ]
    }

    fn full_year_words(&self) -> &'static [&'static str] {
        &["full year"]
    }

    fn document_words(&self) -> &'static [&'static str] {
        &["annual report", "20-f"]
    }

    fn results_words(&self) -> &'static [&'static str] {
        &[
            "sales",
            "profit",
            "revenue",
            "results",
            "financial report",
            "financial statement",
        ]
    }

    fn quarter_phrases(&self) -> &'static [(&'static str, u8)] {
        &[
            ("first quarter", 1),
            ("second quarter", 2),
            ("third quarter", 3),
            ("fourth quarter", 4),
            ("first three months", 1),
            ("first half", 2),
            ("first six months", 2),
            ("half-year report", 2),
            ("half year financial report", 2),
            ("half-year review", 2),
            ("first nine months", 3),
        ]
    }

    fn month_names(&self) -> &'static [&'static str] {
        &MONTHS
    }

    /// "H1 2026": the half-year report is the second quarter's.
    fn extra_quarter_hints(&self, lower: &str) -> Vec<u8> {
        if has_h1_token(lower) {
            vec![2]
        } else {
            Vec::new()
        }
    }
}
