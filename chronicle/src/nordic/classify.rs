//! Classification of MFN news items: periodic report vs. not, corrections,
//! financial-calendar notices and the fiscal period.
//!
//! The classifier is pure and table-tested because a wrong `report_kind`
//! silently creates (or removes) a price reaction downstream. The vocabulary
//! lives in [`super::lang`], one file per language; every language is asked and
//! the answers are combined (any language matches / union of quarter hints), so
//! no list's order matters.

use super::lang::{
    any_contains, fiscal_period_in, has_quarter_token, languages, month_range, starts_a_word,
    years, RangePeriod, ReportLanguage,
};

const MAX_PREFIX_CHARS: usize = 80;
const MIN_REMAINDER_CHARS: usize = 15;

/// Kind of a periodic report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    /// Quarterly, half-year or other interim report.
    Interim,
    /// Annual / year-end results release.
    Annual,
}

impl ReportKind {
    /// Value published as `SpecialEvent.report_kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            ReportKind::Interim => "interim",
            ReportKind::Annual => "annual",
        }
    }
}

/// Everything derived from an item's tags and title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    /// `None` for anything that is not a results release.
    pub report_kind: Option<ReportKind>,
    /// `Correction:` / `Rättelse:` notice.
    pub is_correction: bool,
    /// Announces a change to the financial calendar.
    pub calendar_update: bool,
    /// `2026-Q3` or `2026`; only when unambiguous.
    pub fiscal_period: Option<String>,
}

/// Remove a leading issuer prefix `"<up to 80 chars>: "`, once, if at least 15
/// characters remain (`"GomSpace A/S: Correction: …"` → `"Correction: …"`).
pub fn strip_issuer_prefix(title: &str) -> &str {
    let t = title.trim();
    if let Some(idx) = t.find(": ") {
        if t[..idx].chars().count() <= MAX_PREFIX_CHARS {
            let rest = t[idx + 2..].trim_start();
            if rest.chars().count() >= MIN_REMAINDER_CHARS {
                return rest;
            }
        }
    }
    t
}

/// True if the headline contains a standalone quarter token `Q1`..`Q4` or a
/// language's word for "quarter".
fn mentions_quarter(lower: &str) -> bool {
    any_contains(lower, |l| l.quarter_words()) || (b'1'..=b'4').any(|d| has_quarter_token(lower, d))
}

/// The headline heuristic: strong report wording, or a quarter mention without
/// business-news wording; never for regulatory notices that only look similar.
fn headline_looks_like_report(lower: &str) -> bool {
    if any_contains(lower, |l| l.non_report_words())
        || languages().any(|l| l.clinical_words().iter().any(|w| starts_a_word(lower, w)))
    {
        return false;
    }
    any_contains(lower, |l| l.report_words())
        || (mentions_quarter(lower) && !any_contains(lower, |l| l.news_words()))
}

/// True for correction notices (`Correction:`, `Rättelse:`, `Rettelse:`,
/// `Korrektion:`, `Oikaisu:`, `Korjaus:`). The prefix is stripped before looking, and the
/// unstripped title is checked too (stripping eats a leading `Correction: `).
pub fn is_correction(title: &str) -> bool {
    fn check(s: &str) -> bool {
        let lower = s.trim().to_lowercase();
        let markers = || languages().flat_map(|l| l.correction_markers().iter().copied());
        if markers().any(|m| lower.starts_with(m)) {
            return true;
        }
        let head: String = lower.chars().take(MAX_PREFIX_CHARS).collect();
        markers().any(|m| head.contains(&format!(" {m}")))
    }
    check(title) || check(strip_issuer_prefix(title))
}

/// True if the headline announces a change to the financial calendar.
pub fn is_calendar_update(headline: &str) -> bool {
    let l = headline.to_lowercase();
    any_contains(&l, |l| l.calendar_phrases())
}

/// [`is_calendar_update`] plus early/delayed publication and postponement
/// notices and each language's own announcement wording.
fn is_calendar_notice(title: &str) -> bool {
    let l = title.to_lowercase();
    is_calendar_update(title)
        || any_contains(&l, |l| l.calendar_notice_phrases())
        || languages().any(|lang| lang.is_calendar_notice(&l))
}

/// Mentions the fourth quarter / the full year.
fn mentions_year_end(lower: &str) -> bool {
    // Q1–Q3 or half-year wording means a "full year" mention is an outlook.
    let early_quarter = (b'1'..=b'3').any(|d| has_quarter_token(lower, d))
        || any_contains(lower, |l| l.early_period_words());
    has_quarter_token(lower, b'4')
        || any_contains(lower, |l| l.year_end_words())
        // Not inside "delårsrapport" / "halvårsrapport".
        || languages().any(|l| l.year_end_word_starts().iter().any(|w| starts_a_word(lower, w)))
        || (!early_quarter && any_contains(lower, |l| l.full_year_words()))
}

/// A year-end results headline on an item tagged only `sub:report:annual`:
/// not the annual report document, one standalone year, and a results word.
fn is_annual_results_headline(lower: &str) -> bool {
    let mut ys = years(lower);
    ys.sort_unstable();
    ys.dedup();
    ys.len() == 1
        && !any_contains(lower, |l| l.document_words())
        && any_contains(lower, |l| l.results_words())
}

/// The languages as a slice, for the shared range and quarter helpers.
fn all_languages() -> Vec<&'static dyn ReportLanguage> {
    languages().collect()
}

/// Rules 3–6 of the classification, for a title already known not to be a
/// calendar notice or correction. `lower` is the lower-cased stripped title.
fn kind_from(tags: &[String], lower: &str) -> Option<ReportKind> {
    let has = |s: &str| tags.iter().any(|t| t == s);
    let interim_tag = tags.iter().any(|t| t.starts_with("sub:report:interim"));
    let annual_tag = has("sub:report:annual");
    let report_test = interim_tag || (has(":regulatory") && headline_looks_like_report(lower));
    let early_quarter_tag = ["q1", "q2", "q3"]
        .iter()
        .any(|q| has(&format!("sub:report:interim:{q}")));

    // 3. Year-end results.
    if has("sub:report:interim:q4")
        || (annual_tag && has("sub:report:interim"))
        || (report_test && !early_quarter_tag && mentions_year_end(lower))
        // The period wins over the word: a Finnish "osavuosikatsaus 1. tammikuuta
        // - 31. joulukuuta" is a full-year report.
        || (report_test && month_range(&all_languages(), lower) == Some(RangePeriod::YearEnd))
    {
        return Some(ReportKind::Annual);
    }
    // 4. The published annual report document is not a results release.
    if annual_tag && !interim_tag {
        // MFN tags a company's year-end *results* `sub:report:annual` alone
        // when it has no Q4 tag (Novo Nordisk: "sales increased by 6% ... in
        // 2025"). Those are told from the document and from unrelated news
        // that carry the tag too by the title: no "annual report" wording, a
        // single year, and a results word.
        return (has(":regulatory") && is_annual_results_headline(lower))
            .then_some(ReportKind::Annual);
    }
    // 5. Interim.
    if report_test {
        return Some(ReportKind::Interim);
    }
    // 6.
    None
}

/// `YYYY-Qn` for interim reports, `YYYY` for year-end reports; never a guess.
fn fiscal_period(tags: &[String], title: &str, kind: ReportKind) -> Option<String> {
    fiscal_period_in(&all_languages(), tags, title, kind).map(|p| p.render())
}

/// Classify an MFN item from its tags and title, applying the rules in order:
/// calendar notice, correction, year-end, annual report document, interim.
pub fn classify_item(tags: &[String], title: &str) -> Classification {
    let t = strip_issuer_prefix(title);
    let calendar_update = is_calendar_notice(t) || is_calendar_notice(title);
    // Regulatory notices that are never reports (and so never report corrections).
    let excluded = {
        let (a, b) = (t.to_lowercase(), title.to_lowercase());
        languages().any(|l| {
            l.hard_non_report_words()
                .iter()
                .any(|w| a.contains(w) || b.contains(w))
        })
    };
    let is_correction = !excluded && is_correction(title);
    let report_kind = if excluded || calendar_update || is_correction {
        None
    } else {
        kind_from(tags, &t.to_lowercase())
    };
    let fiscal_period = report_kind.and_then(|k| fiscal_period(tags, t, k));
    Classification {
        report_kind,
        is_correction,
        calendar_update,
        fiscal_period,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ReportKind::{Annual, Interim};

    fn month_range(lower: &str) -> Option<RangePeriod> {
        super::super::lang::month_range(&all_languages(), lower)
    }

    fn month_of(word: &str, next: Option<char>) -> Option<u8> {
        super::super::lang::month_of(&all_languages(), word, next)
    }

    fn tags(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| s.to_string()).collect()
    }

    const REG: &str = ":regulatory";
    const REPORT: &str = "sub:report";
    const INTERIM: &str = "sub:report:interim";
    const Q4: &str = "sub:report:interim:q4";
    const ANNUAL: &str = "sub:report:annual";

    #[test]
    fn strips_issuer_prefix_once_when_enough_remains() {
        assert_eq!(
            strip_issuer_prefix("GomSpace A/S: Correction: GomSpace Reports Strong Q2"),
            "Correction: GomSpace Reports Strong Q2"
        );
        assert_eq!(
            strip_issuer_prefix("AB Volvo: Volvo Group - the fourth quarter"),
            "Volvo Group - the fourth quarter"
        );
        // Remainder too short → untouched.
        assert_eq!(strip_issuer_prefix("Volvo: Q2 report"), "Volvo: Q2 report");
        // Prefix longer than 80 chars → untouched.
        let long = format!("{}: remainder that is long enough", "x".repeat(81));
        assert_eq!(strip_issuer_prefix(&long), long);
        assert_eq!(
            strip_issuer_prefix("no colon here at all"),
            "no colon here at all"
        );
    }

    /// (case, tags, title, kind, correction, calendar)
    #[test]
    fn table() {
        type Row = (
            &'static str,
            Vec<String>,
            &'static str,
            Option<ReportKind>,
            bool,
            bool,
        );
        let rows: Vec<Row> = vec![
            // 1. tags are incomplete: untagged GomSpace year-end report
            ("untagged Q4", tags(&[REG]), "GomSpace A/S: GomSpace Reports an all-time high Q4 and a full year of significant growth", Some(Annual), false, false),
            ("untagged year-end 2024", tags(&[REG]), "GomSpace A/S: GomSpace shows positive free cash flow during Q4 2023 report", Some(Annual), false, false),
            // 2. tag vocabulary
            ("interim tag", tags(&[REG, REPORT, INTERIM]), "Q2 Financial Reporting Confirms a Strong First Half of 2026: 30% Growth, Positive EBIT", Some(Interim), false, false),
            ("interim q-tag", tags(&[REG, REPORT, INTERIM, "sub:report:interim:q2"]), "Hexagon AB: Hexagon Q2 2026 results announced today", Some(Interim), false, false),
            ("volvo year-end annual+interim+q4", tags(&[REG, REPORT, ANNUAL, INTERIM, Q4]), "AB Volvo: Volvo Group - the fourth quarter and full year 2025", Some(Annual), false, false),
            ("annual+interim without q4", tags(&[REG, REPORT, ANNUAL, INTERIM]), "Issuer AB: Results presentation for the period", Some(Annual), false, false),
            ("annual report document (Volvo)", tags(&[REG, REPORT, ANNUAL]), "AB Volvo: Volvo Group publishes Annual Report 2025", None, false, false),
            ("annual report document (GomSpace 2019)", tags(&[REG, REPORT, ANNUAL]), "GomSpace A/S: GomSpace (provider of nanosatellites) announces its annual report for 2018", None, false, false),
            ("annual-only tag but year-end wording", tags(&[REG, REPORT, ANNUAL]), "Issuer AB: Year-end report 2025 and proposed dividend now out", Some(Annual), false, false),
            // 3. language pairs: both are reports
            ("sv interim", tags(&[REG, REPORT, INTERIM]), "Volvokoncernen - det andra kvartalet 2026", Some(Interim), false, false),
            ("en interim", tags(&[REG, REPORT, INTERIM]), "Volvo Group - the second quarter 2026", Some(Interim), false, false),
            // 4. issuer prefix
            ("prefixed correction", tags(&[REG, REPORT, INTERIM]), "GomSpace A/S: Correction: GomSpace Reports Strong Q2 Performance with Growth", None, true, false),
            ("unprefixed correction", tags(&[REG, REPORT, INTERIM]), "Correction: GomSpace Reports Strong Q2 Performance", None, true, false),
            ("rättelse", tags(&[REG, REPORT, INTERIM]), "Issuer AB: Rättelse: Delårsrapport januari-mars", None, true, false),
            ("correction of non-report", tags(&[REG]), "GomSpace A/S: Correction: GomSpace signs a 19.5 MEUR contract", None, true, false),
            // 5. same-day re-sends look like ordinary reports here (handled in SQL)
            ("re-send", tags(&[REG, REPORT, ANNUAL, INTERIM, Q4]), "GomSpace A/S: GomSpace (provider of nanosatellites) announces its quarterly results for the fourth quarter of 2018", Some(Annual), false, false),
            // 6. calendar notices, even with report tags or the word "report"/"results"
            ("financial calendar", tags(&[REG]), "GomSpace A/S: GomSpace updates its financial calendar", None, false, true),
            ("early publication quarterly", tags(&[REG]), "GomSpace A/S: GomSpace: Early publication of quarterly results for the fourth quarter 2023", None, false, true),
            ("early publication year-end tagged", tags(&[REG, REPORT, INTERIM, Q4]), "Issuer AB: Early publication of year-end report Q4 2017", None, false, true),
            ("delayed publication", tags(&[REG, REPORT, ANNUAL]), "Issuer AB: Delayed publication of annual report", None, false, true),
            ("postpone", tags(&[REG]), "Issuer AB: Issuer postpones the interim report for Q3", None, false, true),
            ("swedish senarelägger", tags(&[REG]), "Issuer AB: Issuer senarelägger publiceringen av delårsrapporten", None, false, true),
            ("flyttar rapport", tags(&[REG]), "Issuer AB: Bolaget flyttar datum för rapport", None, false, true),
            // observed in real data
            ("moves up publication", tags(&[REG]), "GomSpace A/S: GomSpace moves up publication of quarterly results for the third quarter 2023 to November 16, 2023", None, false, true),
            ("brings forward", tags(&[REG]), "AB Volvo: The Volvo Group brings forward the report for the third quarter to October 16", None, false, true),
            ("new date for report", tags(&[REG]), "GomSpace A/S: GomSpace issues a market update highlighting a positive free cashflow in Q4, along with a new date for the Q4 2023 interim report", None, false, true),
            ("reporting practices", tags(&[REG]), "GomSpace A/S: GomSpace is changing reporting practices for more efficient and concise market updates", None, false, false),
            ("update on reporting", tags(&[REG]), "AB Volvo: Update on the Volvo Group's financial reporting 2020", None, false, false),
            ("preliminary results are a release", tags(&[REG]), "AB Volvo: The Volvo Group reports preliminary financial results for the first quarter 2023", Some(Interim), false, false),
            // not reports
            ("order intake", tags(&[REG]), "GomSpace A/S: GomSpace Secures 1.8 MEUR (19.9 MSEK) of Products order intake in Q1 2026", None, false, false),
            ("market guidance", tags(&[REG]), "GomSpace A/S: GomSpace announces 2026 market guidance with strong profitable growth", None, false, false),
            ("AGM notice is not a report", tags(&[REG]), "Issuer AB: Notice of annual general meeting in Issuer AB", None, false, false),
            ("AGM decisions", tags(&[REG]), "Issuer AB: Decisions of the annual general meeting 2026", None, false, false),
            ("sustainability report", tags(&[REG]), "Issuer AB: Issuer publishes sustainability report 2025", None, false, false),
            ("conference call invitation", tags(&[REG]), "Issuer AB: Invitation to conference call for the Q3 report", None, false, false),
            ("untagged annual report publication", tags(&[REG]), "Issuer AB: Issuer publishes Annual Report 2025", None, false, false),
            ("not regulatory", tags(&[]), "Issuer AB: Issuer reports strong Q2 growth in a press release", None, false, false),
            ("interim Q3 with full year outlook", tags(&[REG, REPORT, INTERIM, "sub:report:interim:q3"]), "Issuer AB: Third quarter 2026 - full year outlook raised", Some(Interim), false, false),
            ("untagged Q3 with full year outlook", tags(&[REG]), "Issuer AB: Issuer Q3 report: full year outlook raised", Some(Interim), false, false),
            // Danish issuers (Novo Nordisk, real titles and tags unless noted)
            ("novo Q2 2026", tags(&[REG, REPORT, INTERIM]), "Novo Nordisk reports adjusted operating profit of DKK 33,389 million for Q2 2026 and raises full-year outlook", Some(Interim), false, false),
            ("novo Q1 2026", tags(&[REG, REPORT, INTERIM]), "Novo Nordisk's adjusted operating profit reached DKK 32,858 million in Q1 2026", Some(Interim), false, false),
            ("novo nine months", tags(&[REG, REPORT, INTERIM]), "Novo Nordisk's sales increased by 12% in Danish kroner and by 15% at CER in the first nine months of 2025; R&D pipeline progress continues", Some(Interim), false, false),
            ("novo year-end, annual tag only", tags(&[REG, REPORT, ANNUAL]), "Novo Nordisk's sales increased by 6% in Danish kroner and by 10% at constant exchange rates to DKK 309.1 billion in 2025 ", Some(Annual), false, false),
            ("novo annual report document", tags(&[REG, REPORT, ANNUAL]), "Novo Nordisk has published its annual report for 2025", None, false, false),
            ("novo SEC filing of the annual report", tags(&[REG, REPORT, ANNUAL]), "Novo Nordisk files annual report with the SEC", None, false, false),
            ("trial news mis-tagged annual", tags(&[REG, REPORT, ANNUAL]), "Novo Nordisk A/S: CagriSema demonstrated superior HbA1c reduction of 1.91%-points and weight loss of 14.2% in adults with type 2 diabetes in the REIMAGINE 2 trial", None, false, false),
            ("novo share repurchase (tagged)", tags(&[REG, "sub:ca", "sub:ca:shares"]), "Novo Nordisk A/S - share repurchase programme", None, false, false),
            ("novo share repurchase, capital S", tags(&[REG, "sub:ca", "sub:ca:shares"]), "Novo Nordisk A/S \u{2013} Share repurchase programme", None, false, false),
            ("novo insider trading notice", tags(&[REG, "sub:ci", "sub:ci:insider"]), "Novo Nordisk A/S: Trading in Novo Nordisk shares by board members, executives and associated persons", None, false, false),
            ("novo AGM", tags(&[REG]), "Resolutions from the Annual General Meeting of Novo Nordisk A/S", None, false, false),
            ("novo voting rights", tags(&[REG]), "Novo Nordisk A/S \u{2013} Total number of voting rights and share capital in Novo Nordisk A/S as of 26 April 2024", None, false, false),
            ("novo major shareholder", tags(&[REG, "sub:ci"]), "Novo Nordisk \u{2013} major shareholder announcement", None, false, false),
            ("novo trial news saying reports (2025)", tags(&[REG]), "Novo Nordisk phase 2 trial with amycretin reports significant weight loss and HbA1c reduction in type 2 diabetes", None, false, false),
            ("novo trial news saying reports (2020)", tags(&[REG]), "Novo Nordisk reports weight loss of 14.9% (16.9% if taken as intended) in STEP 1 trial", None, false, false),
            ("industrial is not a trial", tags(&[REG]), "Industrial AB: Industrial AB reports strong Q2 2026", Some(Interim), false, false),
            ("novo outlook update is business news", tags(&[REG]), "Novo Nordisk raises adjusted sales and adjusted operating profit outlook for 2026", None, false, false),
            // exclusions win over report wording and over report tags
            ("PDMR notice with the word reporting", tags(&[REG]), "Novo Nordisk A/S \u{2013} Reporting of transactions made by persons discharging managerial responsibilities in Novo Nordisk A/S", None, false, false),
            ("PDMR notice, interim tag", tags(&[REG, REPORT, INTERIM]), "Novo Nordisk A/S \u{2013} Reporting of transactions made by persons discharging managerial responsibilities in Novo Nordisk A/S", None, false, false),
            ("managerial responsibilities, Q3 mention", tags(&[REG]), "Issuer A/S: Managerial responsibilities: transactions in Q3", None, false, false),
            ("danish leading employees", tags(&[REG, REPORT, INTERIM]), "Issuer A/S: Indberetning af transaktioner foretaget af ledende medarbejdere i Issuer A/S", None, false, false),
            ("share buy-back", tags(&[REG, REPORT, INTERIM]), "Issuer A/S: Share buy-back programme, transactions in week 12 of the first quarter", None, false, false),
            ("danish aktietilbagek\u{f8}b", tags(&[REG]), "Issuer A/S: Aktietilbagek\u{f8}b i 3. kvartal", None, false, false),
            ("danish tilbagek\u{f8}bsprogram", tags(&[REG, REPORT, INTERIM]), "Issuer A/S: Tilbagek\u{f8}bsprogram - halv\u{e5}rsrapport", None, false, false),
            ("storaktion\u{e6}r", tags(&[REG]), "Issuer A/S: Meddelelse fra storaktion\u{e6}r", None, false, false),
            ("stemmerettigheder", tags(&[REG]), "Issuer A/S: Stemmerettigheder og kapital i Q2", None, false, false),
            ("total voting rights and capital", tags(&[REG, REPORT, INTERIM]), "Issuer A/S: Total number of voting rights and capital", None, false, false),
            ("correction of an excluded notice is no report correction", tags(&[REG]), "Correction: Novo Nordisk A/S - share repurchase programme", None, false, false),
            // Danish report words
            ("danish delårsrapport", tags(&[REG]), "Issuer A/S: Delårsrapport for 1. kvartal 2026", Some(Interim), false, false),
            ("danish halvårsrapport", tags(&[REG]), "Issuer A/S: Halvårsrapport 2026", Some(Interim), false, false),
            ("danish kvartalsrapport", tags(&[REG]), "Issuer A/S: Kvartalsrapport for 3. kvartal 2026", Some(Interim), false, false),
            ("danish regnskabsmeddelelse", tags(&[REG]), "Issuer A/S: Regnskabsmeddelelse for 2. kvartal 2026", Some(Interim), false, false),
            ("danish årsrapport", tags(&[REG]), "Issuer A/S: Årsrapport 2025", Some(Annual), false, false),
            ("danish årsregnskabsmeddelelse", tags(&[REG]), "Issuer A/S: Årsregnskabsmeddelelse 2025", Some(Annual), false, false),
            ("danish kvartal wording only", tags(&[REG]), "Issuer A/S: Resultat for 2. kvartal 2026", Some(Interim), false, false),
            ("english financial statement for", tags(&[REG]), "Issuer A/S: Financial statement for 2025", Some(Annual), false, false),
            ("danish kvartal with news wording", tags(&[REG]), "Issuer A/S: Ny kontrakt og ordreindgang i 2. kvartal 2026 contract", None, false, false),
            // Danish corrections and calendar notices
            ("danish rettelse", tags(&[REG, REPORT, INTERIM]), "Issuer A/S: Rettelse: Delårsrapport for 1. kvartal 2026", None, true, false),
            ("danish korrektion", tags(&[REG, REPORT, INTERIM]), "Korrektion: Halvårsrapport 2026", None, true, false),
            ("danish finanskalender", tags(&[REG]), "Issuer A/S: Opdateret finanskalender 2026", None, false, true),
            ("danish finansiel kalender", tags(&[REG]), "Issuer A/S: Ændring i finansiel kalender for 2026", None, false, true),
            ("danish ændret dato", tags(&[REG]), "Issuer A/S: Ændret dato for offentliggørelse af delårsrapport", None, false, true),
            // Finnish issuers (real Metso / Neles titles and tags unless noted)
            ("fi interim", tags(&[REG, REPORT, INTERIM]), "Metson osavuosikatsaus 1. tammikuuta - 30. syyskuuta 2026", Some(Interim), false, false),
            ("en interim", tags(&[REG, REPORT, INTERIM]), "Metso's Interim Report January 1 - September 30, 2026", Some(Interim), false, false),
            ("fi half-year", tags(&[REG, REPORT, INTERIM]), "Metson puolivuosikatsaus 1. tammikuuta - 30. kesäkuuta 2026", Some(Interim), false, false),
            ("en half-year", tags(&[REG, REPORT, INTERIM]), "Metso's Half-Year Report January 1 - June 30, 2026", Some(Interim), false, false),
            ("fi interim, prefixed", tags(&[REG, REPORT, INTERIM]), "Metso Corporation: Metson osavuosikatsaus 1. tammikuuta - 31. maaliskuuta 2026", Some(Interim), false, false),
            ("fi financial statements release, untagged", tags(&[REG]), "Metso Corporation: Metson tilinpäätöstiedote 1. tammikuuta - 31. joulukuuta 2025", Some(Annual), false, false),
            ("en financial statements review, untagged", tags(&[REG]), "Metso Corporation: Metso's Financial Statements Review January 1 - December 31, 2025", Some(Annual), false, false),
            ("fi liiketoimintakatsaus", tags(&[REG]), "Issuer Oyj: Liiketoimintakatsaus 1. tammikuuta - 31. maaliskuuta 2026", Some(Interim), false, false),
            ("en business review", tags(&[REG]), "Issuer Oyj: Business Review January 1 - March 31, 2026", Some(Interim), false, false),
            ("fi annual report document", tags(&[REG, REPORT, ANNUAL]), "Metso Corporation: Metson vuosikertomus 2025 on julkistettu", None, false, false),
            ("fi annual report document, untagged", tags(&[REG]), "Issuer Oyj: Vuosikertomus 2025 julkaistu", None, false, false),
            ("fi neles oddity: osavuosikatsaus for the full year", tags(&[]), "Neles Oyj: Neleksen osavuosikatsaus 1. tammikuuta - 31. joulukuuta, 2021", None, false, false),
            ("fi neles oddity with regulatory tag", tags(&[REG]), "Neles Oyj: Neleksen osavuosikatsaus 1. tammikuuta - 31. joulukuuta, 2021", Some(Annual), false, false),
            ("fi nokia quarter ordinal", tags(&[REG]), "Nokia Oyj:n vuoden 2026 kolmannen neljänneksen osavuosikatsaus", Some(Interim), false, false),
            // managers' transactions are never reports, with or without report words
            ("fi managers' transactions", tags(&[REG]), "Metso Corporation: Metso Oyj: Johtohenkilöiden liiketoimet - Niko Pakalén", None, false, false),
            ("en managers' transactions", tags(&[REG]), "Metso Corporation: Managers' transactions - Sami Takaluoma", None, false, false),
            ("en managers' transactions, curly apostrophe", tags(&[REG]), "Orion Corporation: Managers\u{2019} transactions \u{2013} Sophie Papa", None, false, false),
            ("fi johdon liiketoimet", tags(&[REG]), "Wärtsilä Oyj Abp - Johdon liiketoimet: Johnstone, Tom", None, false, false),
            ("managers' transactions with a report word and tags", tags(&[REG, REPORT, INTERIM]), "Issuer Oyj: Managers' transactions - Q3 interim report shares", None, false, false),
            ("fi liiketoimet vs liiketoimintakatsaus", tags(&[REG]), "Issuer Oyj: Johtohenkilöiden liiketoimet osavuosikatsauksen jälkeen", None, false, false),
            ("fi buyback", tags(&[REG]), "Issuer Oyj: Omien osakkeiden hankinta 12.3.2026", None, false, false),
            ("fi buyback ostot", tags(&[REG, REPORT, INTERIM]), "Issuer Oyj: Omien osakkeiden ostot osavuosikatsauskaudella", None, false, false),
            ("fi flagging", tags(&[REG]), "Issuer Oyj: Liputusilmoitus (Issuer Oyj)", None, false, false),
            ("fi flagging formula", tags(&[REG, REPORT, INTERIM]), "Issuer Oyj: Ilmoitus arvopaperimarkkinalain 9 luvun 5 §:n mukaisesti osavuosikatsaus", None, false, false),
            ("fi general meeting", tags(&[REG]), "Issuer Oyj: Yhtiökokouksen päätökset", None, false, false),
            ("fi notice of meeting", tags(&[REG]), "Issuer Oyj: Kutsu varsinaiseen yhtiökokoukseen", None, false, false),
            ("en share buyback", tags(&[REG]), "Issuer Oyj: Share buyback 12 March 2026 report", None, false, false),
            // Finnish corrections and calendar notices
            ("fi oikaisu", tags(&[REG, REPORT, INTERIM]), "Issuer Oyj: Oikaisu: osavuosikatsaus 1. tammikuuta - 31. maaliskuuta 2026", None, true, false),
            ("fi korjaus", tags(&[REG, REPORT, INTERIM]), "Korjaus: osavuosikatsaus 1. tammikuuta - 31. maaliskuuta 2026", None, true, false),
            ("fi neles julkaisee", tags(&[REG]), "Neles Oyj: Neles julkaisee tammi-joulukuun 2021 tilinpäätöstiedotteen 4. helmikuuta 2022", None, false, true),
            ("fi julkaisee osavuosikatsauksen", tags(&[REG]), "Neles Oyj: Neles julkaisee tammi-syyskuun 2021 osavuosikatsauksen 27. lokakuuta 2021", None, false, true),
            ("fi julkistaa possessive", tags(&[REG]), "Metso julkistaa tammi-syyskuun 2018 osavuosikatsauksensa perjantaina 26.10.2018", None, false, true),
            ("fi julkaiseminen", tags(&[REG]), "Outotecin tammi-syyskuun osavuosikatsauksen 2019 julkaiseminen", None, false, true),
            ("en to publish its", tags(&[REG]), "Neles Oyj: Neles to publish its Financial Statements Review for January-December 2021 on February 4, 2022", None, false, true),
            ("en publishing of", tags(&[REG]), "Publishing of Outotec's Interim Report January-March 2019", None, false, true),
            ("en kone publishes on", tags(&[REG]), "KONE publishes January-March 2018 Interim Report on Wednesday, April 25, 2018 at 12:30 p.m. EEST", None, false, true),
            // not reports although they say "report", "interim" or name a quarter
            ("en restated comparatives", tags(&[REG]), "Metso Oyj: Metso updates its segment reporting and publishes restated information based on the new structure", None, false, false),
            ("fi restated comparatives", tags(&[REG]), "Metso Corporation: Metso julkaisee oikaistut vertailuluvut liittyen jatkuvien toimintojen uudelleenluokitteluun", None, false, false),
            ("en reporting schedule", tags(&[REG]), "KONE Oyj: Financial reporting schedule of KONE Corporation in 2027", None, false, false),
            ("en interim president", tags(&[REG]), "Neles Oyj: Simo Sääskilahti, CFO of Neles, appointed Neles' interim President and CEO as of January 1, 2022", None, false, false),
            ("en profit warning naming a quarter", tags(&[REG]), "Neste raises its second quarter 2022 outlook", None, false, false),
            ("fi tulosvaroitus naming a quarter", tags(&[REG]), "Positiivinen tulosvaroitus: UPM täsmentää tulosnäkymiään kolmannen neljänneksen ennakkotietojen pohjalta", None, false, false),
            ("en annual review document", tags(&[REG]), "KONE Oyj: KONE Corporation's Annual Review 2019 has been published", None, false, false),
        ];
        for (name, tags, title, kind, corr, cal) in rows {
            let c = classify_item(&tags, title);
            assert_eq!(c.report_kind, kind, "{name}: kind");
            assert_eq!(c.is_correction, corr, "{name}: correction");
            assert_eq!(c.calendar_update, cal, "{name}: calendar");
        }
    }

    #[test]
    fn fiscal_period_rules() {
        let f = |t: &[&str], title: &str| classify_item(&tags(t), title).fiscal_period;
        assert_eq!(
            f(&[REG, INTERIM], "Volvo Group - the second quarter 2026").as_deref(),
            Some("2026-Q2")
        );
        assert_eq!(
            f(&[REG, INTERIM], "Volvokoncernen - det andra kvartalet 2026").as_deref(),
            Some("2026-Q2")
        );
        assert_eq!(
            f(
                &[REG, INTERIM, "sub:report:interim:q3"],
                "Issuer AB: Results for the period 2025"
            )
            .as_deref(),
            Some("2025-Q3")
        );
        assert_eq!(
            f(&[REG, INTERIM], "Issuer AB: Issuer Q1 2027 report").as_deref(),
            Some("2027-Q1")
        );
        // Year-end → YYYY
        assert_eq!(
            f(
                &[REG, ANNUAL, INTERIM, Q4],
                "AB Volvo: Volvo Group - the fourth quarter and full year 2025"
            )
            .as_deref(),
            Some("2025")
        );
        // Never guess: no year, two years, no quarter.
        assert_eq!(
            f(
                &[REG],
                "GomSpace A/S: GomSpace Reports an all-time high Q4 and a full year of growth"
            ),
            None
        );
        assert_eq!(
            f(
                &[REG, INTERIM],
                "Issuer AB: Q2 2026 report compared with 2025"
            )
            .as_deref(),
            None
        );
        assert_eq!(
            f(
                &[REG, INTERIM],
                "Issuer AB: Interim report for the period 2026"
            ),
            None
        );
        assert_eq!(f(&[REG, INTERIM], "Issuer AB: Q1 and Q2 report 2026"), None);
        // Q1-Q4 with a year, ordinals with "of", Danish ordinals, H1.
        assert_eq!(
            f(&[REG, INTERIM], "Issuer: results for Q3 2026").as_deref(),
            Some("2026-Q3")
        );
        assert_eq!(
            f(
                &[REG, INTERIM],
                "Issuer: results for the first quarter of 2026"
            )
            .as_deref(),
            Some("2026-Q1")
        );
        assert_eq!(
            f(
                &[REG, INTERIM],
                "Issuer: results for the fourth quarter of 2026"
            )
            .as_deref(),
            Some("2026") // the fourth quarter is the year-end report
        );
        assert_eq!(
            f(&[REG], "Issuer A/S: Delårsrapport for 1. kvartal 2026").as_deref(),
            Some("2026-Q1")
        );
        assert_eq!(
            f(&[REG], "Issuer A/S: Kvartalsrapport 3. kvartal 2026").as_deref(),
            Some("2026-Q3")
        );
        assert_eq!(
            f(&[REG], "Issuer A/S: Halvårsrapport H1 2026").as_deref(),
            Some("2026-Q2")
        );
        assert_eq!(
            f(&[REG], "Issuer: H1 2026 report").as_deref(),
            Some("2026-Q2")
        );
        assert_eq!(
            f(
                &[REG, INTERIM],
                "Issuer sales rose in the first nine months of 2025"
            )
            .as_deref(),
            Some("2025-Q3")
        );
        // No unique year → NULL; two quarters → NULL.
        assert_eq!(f(&[REG, INTERIM], "Issuer: Q2 report"), None);
        assert_eq!(f(&[REG, INTERIM], "Issuer: H1 and Q3 2026"), None);
        assert_eq!(f(&[REG, INTERIM], "Issuer: H1 2025 and 2026 report"), None);
        // Annual (year-end) results by statement title.
        assert_eq!(
            f(&[REG], "Issuer A/S: Financial statement for 2025").as_deref(),
            Some("2025")
        );
        // Non-reports have none.
        assert_eq!(
            f(
                &[REG],
                "Issuer AB: Issuer updates its financial calendar for 2026"
            ),
            None
        );
    }

    #[test]
    fn years_are_standalone_tokens() {
        assert_eq!(years("Q2 2026 vs 2025"), vec!["2026", "2025"]);
        assert!(years("id 12026 and 20267").is_empty());
        assert!(years("version2026").is_empty());
    }

    #[test]
    fn is_correction_cases() {
        assert!(is_correction("Correction: GomSpace Reports Strong Q2"));
        assert!(is_correction("  rättelse: Delårsrapport"));
        assert!(is_correction(
            "GomSpace A/S: Correction: GomSpace Reports Strong Q2"
        ));
        assert!(is_correction("Issuer AB - correction: updated figures"));
        assert!(!is_correction("GomSpace corrects nothing"));
        let late = format!(
            "Issuer AB: {} correction: only after eighty characters",
            "word ".repeat(20)
        );
        assert!(!is_correction(&late));
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

    #[test]
    fn quarter_token_needs_word_boundaries() {
        assert!(has_quarter_token("q2 results", b'2'));
        assert!(has_quarter_token("results in q4", b'4'));
        assert!(!has_quarter_token("faq2 results", b'2'));
        assert!(!has_quarter_token("q25 results", b'2'));
        assert!(mentions_quarter("första kvartalet"));
    }

    #[test]
    fn finnish_and_english_ranges_give_the_fiscal_period() {
        let f = |title: &str| classify_item(&tags(&[REG, INTERIM]), title).fiscal_period;
        // Year to date: Q1-Q3.
        assert_eq!(
            f("Metson osavuosikatsaus 1. tammikuuta - 30. syyskuuta 2026").as_deref(),
            Some("2026-Q3")
        );
        assert_eq!(
            f("Metso's Interim Report January 1 - September 30, 2026").as_deref(),
            Some("2026-Q3")
        );
        assert_eq!(
            f("Metson osavuosikatsaus 1. tammikuuta - 31. maaliskuuta 2026").as_deref(),
            Some("2026-Q1")
        );
        // Half year.
        assert_eq!(
            f("Metsolta tammi-kesäkuulta 2026 puolivuosikatsaus").as_deref(),
            Some("2026-Q2")
        );
        assert_eq!(
            f("Metson puolivuosikatsaus 1. tammikuuta - 30. kesäkuuta 2026").as_deref(),
            Some("2026-Q2")
        );
        assert_eq!(
            f("Metso Outotecin tammi-kesäkuun 2022 puolivuosikatsaus").as_deref(),
            Some("2022-Q2")
        );
        assert_eq!(
            f("Metso Outotec's Interim Report January - March 2023").as_deref(),
            Some("2023-Q1")
        );
        assert_eq!(
            f("Metso Outotecin osavuosikatsaus tammi-syyskuu 2021").as_deref(),
            Some("2021-Q3")
        );
        // A single quarter written as a range.
        assert_eq!(
            f("Issuer Oyj: osavuosikatsaus huhti-kesäkuu 2026").as_deref(),
            Some("2026-Q2")
        );
        // Numeric ranges, Swedish months, Q-spans.
        assert_eq!(
            f("Metson osavuosikatsaus 1.1. - 30.9.2013").as_deref(),
            Some("2013-Q3")
        );
        assert_eq!(
            f("Metson osavuosikatsaus 1.1.-30.6.2011").as_deref(),
            Some("2011-Q2")
        );
        assert_eq!(
            f("Wärtsiläs delårsrapport januari-mars 2020").as_deref(),
            Some("2020-Q1")
        );
        assert_eq!(
            f("Outotec's Q1-Q3 2012 Interim Report").as_deref(),
            Some("2012-Q3")
        );
        // Year-end ranges are annual reports with the year.
        let a = |t: &[&str], title: &str| classify_item(&tags(t), title);
        for title in [
            "Metson tilinpäätöstiedote 1. tammikuuta - 31. joulukuuta 2025",
            "Metso's Financial Statements Review January 1 - December 31, 2025",
            "Metson tilinpäätöstiedote tammi-joulukuun 2025",
            "Metsolta tammi-joulukuu 2025 osavuosikatsaus",
            "Issuer: report 1.1. - 31.12.2025",
        ] {
            let c = a(&[REG], title);
            assert_eq!(
                (c.report_kind, c.fiscal_period.as_deref()),
                (Some(Annual), Some("2025")),
                "{title}"
            );
        }
        // The period wins over the word and over an early-quarter tag.
        let c = a(
            &[REG, REPORT, INTERIM, "sub:report:interim:q2"],
            "Neleksen osavuosikatsaus 1. tammikuuta - 31. joulukuuta, 2021",
        );
        assert_eq!(
            (c.report_kind, c.fiscal_period.as_deref()),
            (Some(Annual), Some("2021"))
        );
        // Two months that are not a range say nothing.
        assert_eq!(
            f("Interim report: dividends paid in January and September 2026"),
            None
        );
    }

    #[test]
    fn every_finnish_month_form_is_recognised() {
        const FORMS: [&str; 12] = [
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
        const SHORT: [&str; 12] = [
            "tammi", "helmi", "maalis", "huhti", "touko", "kesä", "heinä", "elo", "syys", "loka",
            "marras", "joulu",
        ];
        for (i, stem) in FORMS.iter().enumerate() {
            let want = Some(i as u8 + 1);
            // nominative, genitive, partitive, ablative, elative.
            for ending in ["", "n", "ta", "lta", "sta"] {
                assert_eq!(
                    month_of(&format!("{stem}{ending}"), None),
                    want,
                    "{stem}{ending}"
                );
            }
            // The short form counts only before a dash.
            assert_eq!(month_of(SHORT[i], Some('-')), want, "{}-", SHORT[i]);
            assert_eq!(month_of(SHORT[i], Some('\u{2013}')), want);
            assert_eq!(month_of(SHORT[i], Some(' ')), None, "{} alone", SHORT[i]);
        }
        assert_eq!(month_of("september", None), Some(9));
        assert_eq!(month_of("maaliskuun", None), Some(3));
        assert_eq!(month_of("kesäkuuta", None), Some(6));
    }

    #[test]
    fn finnish_range_forms_by_month() {
        // Every Jan-to-quarter-end range in the genitive and the partitive.
        for (end, n) in [("maaliskuuta", 1), ("kesäkuuta", 2), ("syyskuuta", 3)] {
            let t = format!("osavuosikatsaus 1. tammikuuta - 30. {end} 2026");
            assert_eq!(month_range(&t), Some(RangePeriod::Quarter(n)), "{t}");
        }
        for (a, b, want) in [
            ("tammi", "joulukuun", Some(RangePeriod::YearEnd)),
            ("tammi", "maaliskuun", Some(RangePeriod::Quarter(1))),
            ("huhti", "kesäkuun", Some(RangePeriod::Quarter(2))),
            ("heinä", "syyskuun", Some(RangePeriod::Quarter(3))),
            ("loka", "joulukuun", Some(RangePeriod::YearEnd)),
            ("tammi", "heinäkuun", None),
        ] {
            assert_eq!(month_range(&format!("{a}-{b} 2026")), want, "{a}-{b}");
        }
    }

    #[test]
    fn finnish_report_words_are_whole_phrases() {
        // `liiketoimet` (transactions) is not `liiketoimintakatsaus` (business review).
        let c = |title: &str| classify_item(&tags(&[REG]), title).report_kind;
        assert_eq!(
            c("Issuer Oyj: Liiketoimintakatsaus 1. tammikuuta - 31. maaliskuuta 2026"),
            Some(Interim)
        );
        assert_eq!(
            c("Issuer Oyj: Johtohenkilöiden liiketoimet - Matti Meikäläinen"),
            None
        );
        assert_eq!(c("Issuer Oyj: Johdon liiketoimet"), None);
        assert!(is_correction(
            "Issuer Oyj: Oikaisu: Tilinpäätöstiedote 2025"
        ));
        assert!(is_correction("  KORJAUS: osavuosikatsaus"));
        assert!(!is_correction(
            "Korjaus Metson 3.2.2017 julkaistuun tilinpäätöstiedotteeseen"
        ));
    }
}
