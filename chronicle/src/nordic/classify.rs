//! Classification of MFN news items: periodic report vs. not, corrections,
//! financial-calendar notices and the fiscal period.
//!
//! The classifier is pure and table-tested because a wrong `report_kind`
//! silently creates (or removes) a price reaction downstream.

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
    // Danish: half-year / annual / quarterly report, results announcement
    // (`årsregnskabsmeddelelse` contains `regnskabsmeddelelse`).
    "halvårsrapport",
    "årsrapport",
    "regnskabsmeddelelse",
    "kvartalsrapport",
    "financial statement for",
];

/// Regulatory notices that are never a results release, even when MFN tags
/// them `sub:report*` or the headline says "report" (a PDMR notice titled
/// "Reporting of transactions made by persons discharging managerial
/// responsibilities"). Unlike [`NON_REPORT_WORDS`] this wins over tags.
const HARD_NON_REPORT_WORDS: &[&str] = &[
    "persons discharging managerial responsibilities",
    "managerial responsibilities",
    "ledende medarbejdere",
    "share repurchase",
    "share buy-back",
    "aktietilbagekøb",
    "tilbagekøbsprogram",
    "major shareholder",
    "storaktionær",
    "voting rights",
    "stemmerettigheder",
    "total number of voting rights and capital",
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

/// Regulatory notices that contain a strong word ("annual general meeting",
/// "sustainability report") but are not results releases. Only consulted by the
/// headline heuristic; explicit `sub:report:interim*` tags are trusted.
const NON_REPORT_WORDS: &[&str] = &[
    "general meeting",
    "bolagsstämma",
    "årsstämma",
    "nomination committee",
    "valberedning",
    "annual report",
    "årsredovisning",
    "sustainability",
    "hållbarhets",
    "remuneration",
    "ersättnings",
    "corporate governance",
    "bolagsstyrning",
    "invitation",
    "inbjudan",
    "conference call",
    "telephone conference",
    "webcast",
    "notice of",
    "notice to",
    "kallelse",
    "repurchase",
    "reporting practices",
    "update on",
    "treasury",
    "återköp",
];

/// Clinical-trial and regulatory-approval wording, matched at the start of a
/// word ("industrial" is not a "trial"). Pharma headlines such as "Novo Nordisk
/// phase 2 trial with amycretin reports significant weight loss" contain
/// "reports" but are not results releases. Only the headline heuristic uses it.
const CLINICAL_WORDS: &[&str] = &[
    "trial", "phase 1", "phase 2", "phase 3", "clinical", "fda", "chmp",
];

const CALENDAR_PHRASES: &[&str] = &[
    "financial calendar",
    "finansiell kalender",
    "finanskalender",
    "finansiel kalender",
    "ændret dato",
    "ändrad rapportdag",
    "changes date of",
    "rescheduled",
    "reporting date",
    "report date",
];

/// Extra phrases for calendar notices that don't use "calendar" wording.
const CALENDAR_NOTICE_PHRASES: &[&str] = &[
    "early publication",
    "delayed publication",
    "postpone",
    "tidigarelägger",
    "senarelägger",
    "brings forward",
    "moves up publication",
    "new date for",
];

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

/// True if `lower` has a standalone token `q<digit>` for the given digit.
fn has_quarter_token(lower: &str, digit: u8) -> bool {
    let b = lower.as_bytes();
    (0..b.len().saturating_sub(1)).any(|i| {
        b[i] == b'q'
            && b[i + 1] == digit
            && (i == 0 || !b[i - 1].is_ascii_alphanumeric())
            && b.get(i + 2).is_none_or(|c| !c.is_ascii_alphanumeric())
    })
}

/// True if the headline contains a standalone quarter token `Q1`..`Q4` or the
/// words `quarter` / `kvartal`.
fn mentions_quarter(lower: &str) -> bool {
    lower.contains("kvartal")
        || lower.contains("quarter")
        || (b'1'..=b'4').any(|d| has_quarter_token(lower, d))
}

/// The headline heuristic: strong report wording, or a quarter mention without
/// business-news wording; never for regulatory notices that only look similar.
fn headline_looks_like_report(lower: &str) -> bool {
    if NON_REPORT_WORDS.iter().any(|w| lower.contains(w))
        || CLINICAL_WORDS.iter().any(|w| starts_a_word(lower, w))
    {
        return false;
    }
    STRONG_WORDS.iter().any(|w| lower.contains(w))
        || (mentions_quarter(lower) && !NEWS_WORDS.iter().any(|w| lower.contains(w)))
}

/// True for correction notices (`Correction:`, `Rättelse:`, `Rettelse:`,
/// `Korrektion:`). The prefix is stripped before looking, and the
/// unstripped title is checked too (stripping eats a leading `Correction: `).
pub fn is_correction(title: &str) -> bool {
    fn check(s: &str) -> bool {
        let lower = s.trim().to_lowercase();
        const MARKERS: [&str; 4] = ["correction:", "rättelse:", "rettelse:", "korrektion:"];
        if MARKERS.iter().any(|m| lower.starts_with(m)) {
            return true;
        }
        let head: String = lower.chars().take(MAX_PREFIX_CHARS).collect();
        MARKERS.iter().any(|m| head.contains(&format!(" {m}")))
    }
    check(title) || check(strip_issuer_prefix(title))
}

/// True if the headline announces a change to the financial calendar.
pub fn is_calendar_update(headline: &str) -> bool {
    let l = headline.to_lowercase();
    CALENDAR_PHRASES.iter().any(|p| l.contains(p))
}

/// [`is_calendar_update`] plus early/delayed publication and postponement notices.
fn is_calendar_notice(title: &str) -> bool {
    let l = title.to_lowercase();
    is_calendar_update(title)
        || CALENDAR_NOTICE_PHRASES.iter().any(|p| l.contains(p))
        || (l.contains("flyttar") && l.contains("rapport"))
}

/// True if `needle` occurs in `lower` at the start of a word.
fn starts_a_word(lower: &str, needle: &str) -> bool {
    lower.match_indices(needle).any(|(i, _)| {
        lower[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphabetic())
    })
}

/// Mentions the fourth quarter / the full year.
fn mentions_year_end(lower: &str) -> bool {
    // Q1–Q3 or half-year wording means a "full year" mention is an outlook.
    let early_quarter = (b'1'..=b'3').any(|d| has_quarter_token(lower, d))
        || [
            "first quarter",
            "second quarter",
            "third quarter",
            "första kvartalet",
            "andra kvartalet",
            "tredje kvartalet",
            "half-year",
            "first half",
            "halvår",
            "1. kvartal",
            "2. kvartal",
            "3. kvartal",
            "first three months",
            "first six months",
            "first nine months",
        ]
        .iter()
        .any(|p| lower.contains(p));
    has_quarter_token(lower, b'4')
        || [
            "fourth quarter",
            "fjärde kvartalet",
            "year-end",
            "bokslutskommuniké",
            "årsregnskabsmeddelelse",
            "financial statement for",
        ]
        .iter()
        .any(|p| lower.contains(p))
        // Not inside "delårsrapport" / "halvårsrapport".
        || starts_a_word(lower, "årsrapport")
        || (!early_quarter && ["full year", "helår"].iter().any(|p| lower.contains(p)))
}

/// A year-end results headline on an item tagged only `sub:report:annual`:
/// not the annual report document, one standalone year, and a results word.
fn is_annual_results_headline(lower: &str) -> bool {
    const DOCUMENT: &[&str] = &["annual report", "årsredovisning", "årsrapport", "20-f"];
    const RESULTS: &[&str] = &[
        "sales",
        "profit",
        "revenue",
        "results",
        "financial report",
        "financial statement",
        "regnskab",
    ];
    let mut ys = years(lower);
    ys.sort_unstable();
    ys.dedup();
    ys.len() == 1
        && !DOCUMENT.iter().any(|w| lower.contains(w))
        && RESULTS.iter().any(|w| lower.contains(w))
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

/// Four-digit years `20xx` that stand alone in the text.
fn years(text: &str) -> Vec<&str> {
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
fn has_h1_token(lower: &str) -> bool {
    let b = lower.as_bytes();
    (0..b.len().saturating_sub(1)).any(|i| {
        b[i] == b'h'
            && b[i + 1] == b'1'
            && (i == 0 || !b[i - 1].is_ascii_alphanumeric())
            && b.get(i + 2).is_none_or(|c| !c.is_ascii_alphanumeric())
    })
}

/// The quarter (1–4) named by a tag or the headline, if exactly one.
fn quarter_of(tags: &[String], lower: &str) -> Option<u8> {
    if let Some(n) = (1..=4u8).find(|n| {
        tags.iter()
            .any(|t| *t == format!("sub:report:interim:q{n}"))
    }) {
        return Some(n);
    }
    let mut found: Vec<u8> = (1..=4u8)
        .filter(|n| has_quarter_token(lower, b'0' + n))
        .collect();
    let ordinals: [(&str, &str, u8); 4] = [
        ("first quarter", "första kvartalet", 1),
        ("second quarter", "andra kvartalet", 2),
        ("third quarter", "tredje kvartalet", 3),
        ("fourth quarter", "fjärde kvartalet", 4),
    ];
    for (en, sv, n) in ordinals {
        if (lower.contains(en) || lower.contains(sv)) && !found.contains(&n) {
            found.push(n);
        }
    }
    // Danish "1. kvartal 2026", English "first/... quarter of 2026" (above),
    // "H1 2026" (the half-year report is the second quarter's), and the
    // cumulative wording of year-to-date reports.
    let extra: [(&[&str], u8); 5] = [
        (&["1. kvartal", "first three months"], 1),
        (&["2. kvartal", "first half", "first six months"], 2),
        (&["3. kvartal", "first nine months"], 3),
        (&["4. kvartal"], 4),
        (&[], 2), // `H1`, below
    ];
    for (needles, n) in extra {
        let hit = if needles.is_empty() {
            has_h1_token(lower)
        } else {
            needles.iter().any(|w| lower.contains(w))
        };
        if hit && !found.contains(&n) {
            found.push(n);
        }
    }
    if found.len() == 1 {
        Some(found[0])
    } else {
        None
    }
}

/// `YYYY-Qn` for interim reports, `YYYY` for year-end reports; never a guess.
fn fiscal_period(tags: &[String], title: &str, kind: ReportKind) -> Option<String> {
    let mut ys = years(title);
    ys.sort_unstable();
    ys.dedup();
    let [year] = ys.as_slice() else { return None };
    match kind {
        ReportKind::Annual => Some((*year).to_string()),
        ReportKind::Interim => {
            quarter_of(tags, &title.to_lowercase()).map(|q| format!("{year}-Q{q}"))
        }
    }
}

/// Classify an MFN item from its tags and title, applying the rules in order:
/// calendar notice, correction, year-end, annual report document, interim.
pub fn classify_item(tags: &[String], title: &str) -> Classification {
    let t = strip_issuer_prefix(title);
    let calendar_update = is_calendar_notice(t) || is_calendar_notice(title);
    // Regulatory notices that are never reports (and so never report corrections).
    let excluded = {
        let (a, b) = (t.to_lowercase(), title.to_lowercase());
        HARD_NON_REPORT_WORDS
            .iter()
            .any(|w| a.contains(w) || b.contains(w))
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
}
