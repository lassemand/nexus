//! Classification of Nasdaq Nordic news items: report vs. non-report,
//! corrections and financial-calendar announcements.

/// `cnsCategory` values that can contain a periodic report. "Interim
/// information" is the older category used before 2024-ish.
pub const REPORT_CATEGORIES: &[&str] = &[
    "Interim report (Q1 and Q3)",
    "Half-year financial report",
    "Annual financial report",
    "Interim information",
];

const ANNUAL_CATEGORY: &str = "Annual financial report";

/// Words that on their own identify a periodic report headline.
const STRONG_WORDS: &[&str] = &[
    "report",
    "interim",
    "half-year",
    "year-end",
    "annual",
    "rapport",
    "bokslutskommuniké",
    "delårsrapport",
];

/// Words that mark a headline as business news that merely mentions a quarter
/// (e.g. "Secures 1.8 MEUR of Products order intake in Q1 2026").
const NEWS_WORDS: &[&str] = &[
    "order intake",
    "guidance",
    "contract",
    "agreement",
    "secures",
    "signs",
    "market update",
];

const CALENDAR_PHRASES: &[&str] = &[
    "financial calendar",
    "finansiell kalender",
    "ändrad rapportdag",
    "changes date of",
    "rescheduled",
    "reporting date",
    "report date",
];

/// Kind of a periodic report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    /// Quarterly, half-year or other interim report.
    Interim,
    /// Annual / year-end report.
    Annual,
}

impl ReportKind {
    /// Value stored in `report_releases.report_kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            ReportKind::Interim => "interim",
            ReportKind::Annual => "annual",
        }
    }
}

/// True if `headline` contains a standalone quarter token such as `Q1`..`Q4`
/// or the words `quarter` / `kvartal`.
fn mentions_quarter(lower: &str) -> bool {
    if lower.contains("kvartal") || lower.contains("quarter") {
        return true;
    }
    let b = lower.as_bytes();
    (0..b.len().saturating_sub(1)).any(|i| {
        b[i] == b'q'
            && (b'1'..=b'4').contains(&b[i + 1])
            && (i == 0 || !b[i - 1].is_ascii_alphanumeric())
            && b.get(i + 2).is_none_or(|c| !c.is_ascii_digit())
    })
}

/// Decide whether an item is a periodic report.
///
/// Rule: the category must be one of [`REPORT_CATEGORIES`], and then
/// 1. category `Annual financial report` → report (always);
/// 2. headline contains a strong report word → report;
/// 3. headline mentions a quarter (`Q1`–`Q4`, `quarter`, `kvartal`) and no business-news
///    word ([`NEWS_WORDS`]) → report;
/// 4. otherwise not a report.
///
/// Rule 3's exclusion list goes beyond the plain "mentions a quarter" rule
/// because Nasdaq files order-intake news under the interim-report category
/// (GomSpace 2026-04-01 "…Products order intake in Q1 2026").
/// Corrections are never reports.
pub fn report_kind(category: &str, headline: &str) -> Option<ReportKind> {
    if !REPORT_CATEGORIES.contains(&category) || is_correction(headline) {
        return None;
    }
    let kind = if category == ANNUAL_CATEGORY {
        ReportKind::Annual
    } else {
        ReportKind::Interim
    };
    if category == ANNUAL_CATEGORY {
        return Some(kind);
    }
    let lower = headline.to_lowercase();
    if STRONG_WORDS.iter().any(|w| lower.contains(w)) {
        return Some(kind);
    }
    if mentions_quarter(&lower) && !NEWS_WORDS.iter().any(|w| lower.contains(w)) {
        return Some(kind);
    }
    None
}

/// True for headlines starting `Correction:` or `Rättelse:` (case-insensitive).
pub fn is_correction(headline: &str) -> bool {
    let l = headline.trim_start().to_lowercase();
    l.starts_with("correction:") || l.starts_with("rättelse:")
}

/// True if the headline announces a change to the financial calendar.
pub fn is_calendar_update(headline: &str) -> bool {
    let l = headline.to_lowercase();
    CALENDAR_PHRASES.iter().any(|p| l.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q: &str = "Interim report (Q1 and Q3)";

    #[test]
    fn real_reports_are_kept() {
        for (cat, h) in [
            (
                Q,
                "Q2 Financial Reporting Confirms a Strong First Half of 2026: 30% Growth",
            ),
            (
                Q,
                "GomSpace reports a solid Q1 with 43% revenue growth and positive EBITDA",
            ),
            (
                Q,
                "GomSpace Reports Strong Q3 Performance with continued Growth",
            ),
            (
                "Interim information",
                "GomSpace Q2 2024 interim and half-year report",
            ),
            ("Interim information", "GomSpace Q1 2024 Trading Statement"),
            (
                "Interim information",
                "GomSpace is reporting positive free cashflow for the 3rd quarter of 2024",
            ),
            (
                "Interim information",
                "GomSpace announces its quarterly results for the first quarter of 2022",
            ),
            (
                "Half-year financial report",
                "Delårsrapport januari-juni 2025",
            ),
        ] {
            assert_eq!(report_kind(cat, h), Some(ReportKind::Interim), "{h}");
        }
    }

    #[test]
    fn annual_category_is_always_a_report() {
        let h = "GomSpace shows positive free cash flow during Q4 2023";
        assert_eq!(
            report_kind("Annual financial report", h),
            Some(ReportKind::Annual)
        );
        assert_eq!(
            report_kind("Annual financial report", "Anything at all"),
            Some(ReportKind::Annual)
        );
    }

    #[test]
    fn non_reports_in_report_categories_are_excluded() {
        assert_eq!(
            report_kind(
                Q,
                "GomSpace Secures 1.8 MEUR (19.9 MSEK) of Products order intake in Q1 2026"
            ),
            None
        );
        assert_eq!(
            report_kind(
                Q,
                "GomSpace announces 2026 market guidance with strong profitable growth"
            ),
            None
        );
        assert_eq!(
            report_kind(
                "Interim information",
                "GomSpace issues a market update highlighting a positive free cashflow in Q2"
            ),
            None
        );
    }

    #[test]
    fn other_categories_never_count() {
        assert_eq!(report_kind("Company Announcement", "Q2 report"), None);
    }

    #[test]
    fn corrections_are_never_reports() {
        assert_eq!(
            report_kind(Q, "Correction: GomSpace Reports Strong Q2 Performance"),
            None
        );
        assert!(is_correction("Correction: GomSpace Reports Strong Q2"));
        assert!(is_correction("  rättelse: Delårsrapport"));
        assert!(!is_correction("GomSpace corrects nothing"));
    }

    #[test]
    fn quarter_token_needs_word_boundaries() {
        assert!(mentions_quarter("q2 results"));
        assert!(mentions_quarter("results in q4"));
        assert!(!mentions_quarter("faq2 results"));
        assert!(!mentions_quarter("q25 results"));
        assert!(mentions_quarter("första kvartalet"));
    }

    #[test]
    fn calendar_matcher() {
        for h in [
            "GomSpace updates its financial calendar",
            "Ändrad rapportdag för delårsrapport",
            "Company changes date of Q3 report",
            "Interim report rescheduled",
            "Uppdaterad finansiell kalender",
        ] {
            assert!(is_calendar_update(h), "{h}");
        }
        for h in [
            "Notice to annual general meeting",
            "GomSpace Reports Strong Q3 Performance",
            "Decisions of general meeting",
        ] {
            assert!(!is_calendar_update(h), "{h}");
        }
    }
}
