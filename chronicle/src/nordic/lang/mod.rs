//! Report-headline vocabulary and fiscal-period parsing, one file per language.
//!
//! A [`ReportLanguage`] holds the words one language uses (report words,
//! non-report words, calendar phrases, correction markers, month names, quarter
//! phrases). It is keyed by **language, not country**: Finnish issuers also
//! publish in Swedish and English, and MFN's calendar is always Swedish.
//!
//! The classifier asks every language and combines the answers with "any
//! language matches" (word lists) or "union of the hints, then exactly one"
//! (quarters). Those combinations are unions, so the order of [`ALL`] does not
//! change any result; the one place where order could matter, a word naming a
//! month in two languages, is excluded by a test. The shared steps (quarter
//! tokens, year detection, "month to month" ranges) are written once here.

use super::classify::ReportKind;
use model::nordic::Lang;

pub mod da;
pub mod en;
pub mod fi;
pub mod sv;

/// What a language contributes to classification. Every list is lower case and
/// matched as a substring of the lower-cased headline unless noted.
pub trait ReportLanguage: Send + Sync {
    /// The language.
    fn lang(&self) -> Lang;

    /// Words that on their own identify a periodic report headline.
    fn report_words(&self) -> &'static [&'static str] {
        &[]
    }
    /// Regulatory notices that are never a results release, even when tagged
    /// as a report. Wins over tags.
    fn hard_non_report_words(&self) -> &'static [&'static str] {
        &[]
    }
    /// Business news that merely mentions a quarter.
    fn news_words(&self) -> &'static [&'static str] {
        &[]
    }
    /// Notices that contain a report word but are not results releases.
    fn non_report_words(&self) -> &'static [&'static str] {
        &[]
    }
    /// Clinical-trial wording, matched at the start of a word.
    fn clinical_words(&self) -> &'static [&'static str] {
        &[]
    }
    /// Words meaning "quarter" (`kvartal`, `quarter`).
    fn quarter_words(&self) -> &'static [&'static str] {
        &[]
    }
    /// Phrases that announce a change to the financial calendar.
    fn calendar_phrases(&self) -> &'static [&'static str] {
        &[]
    }
    /// Further phrases of early/delayed publication and postponement notices.
    fn calendar_notice_phrases(&self) -> &'static [&'static str] {
        &[]
    }
    /// Calendar-notice wording that needs more than a phrase list, such as
    /// "announces + report word" combinations.
    fn is_calendar_notice(&self, _lower: &str) -> bool {
        false
    }
    /// Correction prefixes (`rättelse:`), lower case.
    fn correction_markers(&self) -> &'static [&'static str] {
        &[]
    }
    /// Wording of the first three quarters or the half year: a "full year"
    /// mention next to it is an outlook, not a year-end report.
    fn early_period_words(&self) -> &'static [&'static str] {
        &[]
    }
    /// Wording of the fourth quarter / year-end report.
    fn year_end_words(&self) -> &'static [&'static str] {
        &[]
    }
    /// Year-end words matched at the start of a word (`årsrapport` must not
    /// match inside `delårsrapport`).
    fn year_end_word_starts(&self) -> &'static [&'static str] {
        &[]
    }
    /// "Full year" wording, a year-end hint unless early-period wording is present.
    fn full_year_words(&self) -> &'static [&'static str] {
        &[]
    }
    /// The annual report *document* (not a results release).
    fn document_words(&self) -> &'static [&'static str] {
        &[]
    }
    /// Words of a results headline.
    fn results_words(&self) -> &'static [&'static str] {
        &[]
    }
    /// Phrases naming one quarter (`(phrase, quarter)`), matched anywhere.
    fn quarter_phrases(&self) -> &'static [(&'static str, u8)] {
        &[]
    }
    /// Month names in order January..December.
    fn month_names(&self) -> &'static [&'static str] {
        &[]
    }
    /// The month (1-12) `word` names in this language. `next` is the character
    /// after the word, for forms that only count before a dash.
    fn month_number(&self, word: &str, _next: Option<char>) -> Option<u8> {
        self.month_names()
            .iter()
            .position(|m| *m == word)
            .map(|i| i as u8 + 1)
    }
    /// Quarter hints specific to the language's wording beyond
    /// [`Self::quarter_phrases`] (e.g. the English `H1` token).
    fn extra_quarter_hints(&self, _lower: &str) -> Vec<u8> {
        Vec::new()
    }
    /// The period of a numeric `d.m. - d.m.` range, for languages that write one.
    fn numeric_range(&self, _lower: &str) -> Option<RangePeriod> {
        None
    }
}

/// Every language, in the fixed order the classifier asks them.
pub const ALL: [Lang; 4] = [Lang::Sv, Lang::Da, Lang::Fi, Lang::En];

/// The implementation of `lang`.
pub fn report_language(lang: Lang) -> &'static dyn ReportLanguage {
    match lang {
        Lang::Sv => &sv::Swedish,
        Lang::Da => &da::Danish,
        Lang::Fi => &fi::Finnish,
        Lang::En => &en::English,
    }
}

/// All implementations, in [`ALL`] order.
pub fn languages() -> impl Iterator<Item = &'static dyn ReportLanguage> {
    ALL.into_iter().map(report_language)
}

/// True if any language's list picked by `words` has an entry in `lower`.
pub fn any_contains(
    lower: &str,
    words: fn(&dyn ReportLanguage) -> &'static [&'static str],
) -> bool {
    languages().any(|l| words(l).iter().any(|w| lower.contains(w)))
}

/// A fiscal period: a year and, for interim reports, the quarter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiscalPeriod {
    /// Four-digit year.
    pub year: String,
    /// 1-4 for interim reports, `None` for year-end.
    pub quarter: Option<u8>,
}

impl FiscalPeriod {
    /// `2026-Q3` or `2026`.
    pub fn render(&self) -> String {
        match self.quarter {
            Some(q) => format!("{}-Q{q}", self.year),
            None => self.year.clone(),
        }
    }
}

impl dyn ReportLanguage {
    /// The fiscal period this language alone reads from `title`; `None` unless
    /// exactly one year (and for interim reports exactly one quarter) is named.
    /// The classifier merges all languages instead; see [`fiscal_period_in`].
    pub fn fiscal_period(
        &self,
        tags: &[String],
        title: &str,
        kind: ReportKind,
    ) -> Option<FiscalPeriod> {
        fiscal_period_in(&[self], tags, title, kind)
    }
}

/// `YYYY-Qn` for interim reports, `YYYY` for year-end reports; never a guess.
/// Quarter hints of all `langs` are merged, so a headline mixing languages
/// reads like the union of its parts.
pub fn fiscal_period_in(
    langs: &[&dyn ReportLanguage],
    tags: &[String],
    title: &str,
    kind: ReportKind,
) -> Option<FiscalPeriod> {
    let mut ys = years(title);
    ys.sort_unstable();
    ys.dedup();
    let [year] = ys.as_slice() else { return None };
    let quarter = match kind {
        ReportKind::Annual => None,
        ReportKind::Interim => Some(quarter_of(langs, tags, &title.to_lowercase())?),
    };
    Some(FiscalPeriod {
        year: (*year).to_string(),
        quarter,
    })
}

/// What a "from month to month" range in a title says about the period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangePeriod {
    /// The first (or only) three, six or nine months, or a single quarter:
    /// quarter 1 to 3.
    Quarter(u8),
    /// January to December (or the last quarter): the year-end report.
    YearEnd,
}

/// True if `lower` has a standalone token `q<digit>` for the given digit.
pub fn has_quarter_token(lower: &str, digit: u8) -> bool {
    let b = lower.as_bytes();
    (0..b.len().saturating_sub(1)).any(|i| {
        b[i] == b'q'
            && b[i + 1] == digit
            && (i == 0 || !b[i - 1].is_ascii_alphanumeric())
            && b.get(i + 2).is_none_or(|c| !c.is_ascii_alphanumeric())
    })
}

/// True if `needle` occurs in `lower` at the start of a word.
pub fn starts_a_word(lower: &str, needle: &str) -> bool {
    lower.match_indices(needle).any(|(i, _)| {
        lower[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphabetic())
    })
}

/// Four-digit years `20xx` that stand alone in the text.
pub fn years(text: &str) -> Vec<&str> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= b.len() {
        let standalone = (i == 0 || !b[i - 1].is_ascii_alphanumeric())
            && b.get(i + 4).is_none_or(|c| !c.is_ascii_alphanumeric());
        if standalone
            && b[i] == b'2'
            && b[i + 1] == b'0'
            && b[i + 2..i + 4].iter().all(u8::is_ascii_digit)
        {
            out.push(&text[i..i + 4]);
            i += 4;
        } else {
            i += 1;
        }
    }
    out
}

/// True if `lower` has a standalone token `h1`.
pub fn has_h1_token(lower: &str) -> bool {
    let b = lower.as_bytes();
    (0..b.len().saturating_sub(1)).any(|i| {
        b[i] == b'h'
            && b[i + 1] == b'1'
            && (i == 0 || !b[i - 1].is_ascii_alphanumeric())
            && b.get(i + 2).is_none_or(|c| !c.is_ascii_alphanumeric())
    })
}

/// The month (1-12) a word names in any language, with `next` the character
/// that follows it.
pub fn month_of(langs: &[&dyn ReportLanguage], word: &str, next: Option<char>) -> Option<u8> {
    langs.iter().find_map(|l| l.month_number(word, next))
}

/// The period named by a "month - month" range: `1. tammikuuta - 30.
/// syyskuuta 2026`, `tammi-kesäkuulta 2026`, `January 1 - September 30, 2026`.
/// Exactly two months joined by a dash (or "to"); anything else is `None`.
pub fn month_range(langs: &[&dyn ReportLanguage], lower: &str) -> Option<RangePeriod> {
    month_name_range(langs, lower).or_else(|| langs.iter().find_map(|l| l.numeric_range(lower)))
}

/// A range of two month names.
fn month_name_range(langs: &[&dyn ReportLanguage], lower: &str) -> Option<RangePeriod> {
    let mut months: Vec<(u8, usize, usize)> = Vec::new();
    let mut start = None;
    let push = |s: usize, e: usize, months: &mut Vec<(u8, usize, usize)>| {
        if let Some(m) = month_of(langs, &lower[s..e], lower[e..].chars().next()) {
            months.push((m, s, e));
        }
    };
    for (i, c) in lower.char_indices() {
        match (c.is_alphabetic(), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                push(s, i, &mut months);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        push(s, lower.len(), &mut months);
    }
    let [(a, _, end_a), (b, start_b, _)] = months.as_slice() else {
        return None;
    };
    let between: String = lower[*end_a..*start_b]
        .chars()
        .filter(|c| !c.is_ascii_digit() && !c.is_whitespace() && !matches!(c, '.' | ','))
        .collect();
    if !matches!(between.as_str(), "-" | "\u{2013}" | "\u{2014}" | "to") {
        return None;
    }
    range_period(*a, *b)
}

/// What "from month `a` to month `b`" is: a year-end, or a quarter.
pub fn range_period(a: u8, b: u8) -> Option<RangePeriod> {
    match (a, b) {
        (1 | 10, 12) => Some(RangePeriod::YearEnd),
        (1, 3 | 6 | 9) | (4, 6) | (7, 9) => Some(RangePeriod::Quarter(b / 3)),
        _ => None,
    }
}

/// The quarter (1–4) named by a tag or the headline, if exactly one.
pub fn quarter_of(langs: &[&dyn ReportLanguage], tags: &[String], lower: &str) -> Option<u8> {
    if let Some(n) = (1..=4u8).find(|n| {
        tags.iter()
            .any(|t| *t == format!("sub:report:interim:q{n}"))
    }) {
        return Some(n);
    }
    let mut found: Vec<u8> = (1..=4u8)
        .filter(|n| has_quarter_token(lower, b'0' + n))
        .collect();
    let mut add = |n: u8| {
        if !found.contains(&n) {
            found.push(n);
        }
    };
    for l in langs {
        for (phrase, n) in l.quarter_phrases() {
            if lower.contains(phrase) {
                add(*n);
            }
        }
        for n in l.extra_quarter_hints(lower) {
            add(n);
        }
    }
    // `Q1-Q3 2012`: the cumulative report of the last quarter named.
    for (span, n) in [
        ("q1-q2", 2),
        ("q1-q3", 3),
        ("q1\u{2013}q2", 2),
        ("q1\u{2013}q3", 3),
    ] {
        if lower.contains(span) {
            return Some(n);
        }
    }
    if let Some(RangePeriod::Quarter(n)) = month_range(langs, lower) {
        if !found.contains(&n) {
            found.push(n);
        }
    }
    if found.len() == 1 {
        Some(found[0])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nordic::classify::classify_item;

    fn period(lang: Lang, title: &str, kind: ReportKind) -> Option<String> {
        report_language(lang)
            .fiscal_period(&[], title, kind)
            .map(|p| p.render())
    }

    #[test]
    fn each_language_reads_its_own_fiscal_periods() {
        use ReportKind::{Annual, Interim};
        let cases = [
            (
                Lang::Fi,
                "Metso osavuosikatsaus 1. tammikuuta - 30. syyskuuta 2026",
                Interim,
                "2026-Q3",
            ),
            (
                Lang::Fi,
                "Osavuosikatsaus tammi-kesäkuulta 2026",
                Interim,
                "2026-Q2",
            ),
            (Lang::Fi, "Tilinpäätöstiedote 2025", Annual, "2025"),
            (
                Lang::Fi,
                "Osavuosikatsaus 1.1. - 30.9.2013",
                Interim,
                "2013-Q3",
            ),
            (
                Lang::Sv,
                "Delårsrapport januari–september 2026",
                Interim,
                "2026-Q3",
            ),
            (
                Lang::Sv,
                "Volvokoncernen - det andra kvartalet 2026",
                Interim,
                "2026-Q2",
            ),
            (
                Lang::Da,
                "Delårsrapport for 1. kvartal 2026",
                Interim,
                "2026-Q1",
            ),
            (Lang::Da, "Årsregnskabsmeddelelse 2025", Annual, "2025"),
            (
                Lang::En,
                "Interim report January-June 2026",
                Interim,
                "2026-Q2",
            ),
            (
                Lang::En,
                "Group results for the first quarter of 2026",
                Interim,
                "2026-Q1",
            ),
            (Lang::En, "H1 2026 results", Interim, "2026-Q2"),
        ];
        for (lang, title, kind, want) in cases {
            assert_eq!(period(lang, title, kind).as_deref(), Some(want), "{title}");
        }
    }

    #[test]
    fn a_language_does_not_read_another_languages_wording() {
        assert_eq!(
            period(
                Lang::Da,
                "Osavuosikatsaus tammi-kesäkuulta 2026",
                ReportKind::Interim
            ),
            None
        );
        assert_eq!(
            period(
                Lang::Fi,
                "Delårsrapport januari–september 2026",
                ReportKind::Interim
            ),
            None
        );
    }

    #[test]
    fn each_language_recognises_its_own_report_headline() {
        let tags = vec![":regulatory".to_string()];
        for (lang, title) in [
            (Lang::Sv, "Bokslutskommuniké 2025"),
            (Lang::Da, "Halvårsrapport 2026 for selskabet"),
            (Lang::Fi, "Puolivuosikatsaus tammi-kesäkuu 2026"),
            (Lang::En, "Half-year report January-June 2026"),
        ] {
            let l = report_language(lang);
            assert!(
                l.report_words()
                    .iter()
                    .any(|w| title.to_lowercase().contains(w)),
                "{title}"
            );
            assert!(classify_item(&tags, title).report_kind.is_some(), "{title}");
        }
    }

    #[test]
    fn no_word_names_a_different_month_in_two_languages() {
        let langs: Vec<_> = languages().collect();
        for l in &langs {
            for (i, name) in l.month_names().iter().enumerate() {
                let hits: Vec<_> = langs
                    .iter()
                    .filter_map(|o| o.month_number(name, None))
                    .collect();
                assert!(hits.iter().all(|m| *m == i as u8 + 1), "{name}: {hits:?}");
            }
        }
    }

    #[test]
    fn every_language_is_keyed_by_its_own_lang() {
        for lang in ALL {
            assert_eq!(report_language(lang).lang(), lang);
        }
    }

    #[test]
    fn every_country_language_has_an_implementation() {
        for c in model::nordic::COUNTRIES {
            for lang in c.languages {
                assert!(ALL.contains(lang), "{}: {lang:?}", c.code);
            }
        }
    }
}
