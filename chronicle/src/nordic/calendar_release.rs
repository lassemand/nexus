//! Report dates from "financial calendar" releases (NEX-219).
//!
//! The MFN company page lists only a curated subset of a company's report
//! dates. Companies announce the full list in a calendar release
//! ("Financial calendar 2026", tag `sub:ci:calendar`) whose body is a list of
//! `date label` lines. [`parse_calendar_release`] turns such an item into
//! [`CalendarEntry`]s. It is pure: no I/O, no clock.
//!
//! Lines are read independently. A line yields an entry when it holds one
//! valid date with a year and its text classifies as a periodic report
//! ([`classify_label`]). Anything it cannot read is skipped, never guessed.

use super::calendar::{classify_label, CalendarEntry, LabelClass};
use super::classify::{is_calendar_update, ReportKind};
use super::feed::NewsItem;
use super::lang::{languages, quarter_of, years, ReportLanguage};
use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Utc};
use tracing::{debug, warn};
use uuid::Uuid;

/// MFN tag of financial calendar releases.
const CALENDAR_TAG: &str = "sub:ci:calendar";

/// Words that introduce a date and belong to it (`den 24 november`).
const DATE_ARTICLES: &[&str] = &["den", "the", "on"];
/// Words that introduce a time and belong to it (`kl. 08:00`).
const TIME_PREFIXES: &[&str] = &["kl", "klo", "kello", "klockan", "at"];
/// Words in a prose line that make an otherwise unlabelled quarter a report.
const REPORT_WORDS: &[&str] = &["report", "rapport", "katsaus", "results", "resultat"];

/// The report dates of one calendar release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseCalendar {
    /// The MFN item the entries come from.
    pub news_id: Uuid,
    /// When the release was published.
    pub published_at: DateTime<Utc>,
    /// Language of the release (`en`, `da`, `sv`, `fi`), when MFN says.
    pub lang: Option<String>,
    /// The dated report lines, in text order. Empty when none could be read.
    pub entries: Vec<CalendarEntry>,
}

/// Parse a financial calendar release.
///
/// Returns `None` unless the item is a calendar release (tag `sub:ci:calendar`
/// or a calendar-update headline). A release with no text, or without a single
/// readable report line, gives `Some` with empty `entries` and a `warn!`.
///
/// `CalendarEntry::time` is the time **as written, in the issuer's local time**
/// (no time zone is attached); the caller decides the time zone. It is `None`
/// unless the line has an explicit `HH:MM` / `HH.MM` time.
pub fn parse_calendar_release(item: &NewsItem) -> Option<ReleaseCalendar> {
    if !item.tags.iter().any(|t| t == CALENDAR_TAG) && !is_calendar_update(&item.title) {
        return None;
    }
    let mut entries: Vec<CalendarEntry> = Vec::new();
    match item.text.as_deref().filter(|t| !t.trim().is_empty()) {
        None => warn!(news_id = %item.news_id, title = %item.title, "calendar release has no text"),
        Some(text) => {
            let langs: Vec<&dyn ReportLanguage> = languages().collect();
            for line in text.lines() {
                if let Some(entry) = parse_line(line, &langs, item) {
                    let dup = entries.iter().any(|e| {
                        e.date == entry.date
                            && e.kind == entry.kind
                            && e.fiscal_period == entry.fiscal_period
                    });
                    if !dup {
                        entries.push(entry);
                    }
                }
            }
            if entries.is_empty() {
                warn!(news_id = %item.news_id, title = %item.title, "no dated report line found in calendar release");
            }
        }
    }
    Some(ReleaseCalendar {
        news_id: item.news_id,
        published_at: item.published_at,
        lang: item.lang.clone(),
        entries,
    })
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(String),
    Word(String),
    Sym(char),
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    start: usize,
    end: usize,
}

fn tokenize(s: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if c.is_whitespace() {
            continue;
        }
        let kind = |c: char| {
            if c.is_ascii_digit() {
                1
            } else if c.is_alphabetic() {
                2
            } else {
                0
            }
        };
        let k = kind(c);
        let mut end = i + c.len_utf8();
        if k != 0 {
            while let Some(&(j, d)) = it.peek() {
                if kind(d) != k {
                    break;
                }
                end = j + d.len_utf8();
                it.next();
            }
        }
        let text = &s[i..end];
        let tok = match k {
            1 => Tok::Num(text.to_string()),
            2 => Tok::Word(text.to_lowercase()),
            _ => Tok::Sym(c),
        };
        out.push(Token { tok, start: i, end });
    }
    out
}

/// A date written in the text; `year` is `None` for the start of a range
/// (`1 January – 30 June 2026`).
#[derive(Debug, Clone)]
struct Mention {
    start: usize,
    end: usize,
    day: u32,
    month: u32,
    year: Option<i32>,
}

impl Mention {
    fn date(&self) -> Option<NaiveDate> {
        NaiveDate::from_ymd_opt(self.year?, self.month, self.day)
    }
}

/// The month (1–12) a word names: a full name or stem in any language, or an
/// unambiguous abbreviation of at least three letters (`nov`, `sept`).
fn month_of_word(langs: &[&dyn ReportLanguage], word: &str) -> Option<u32> {
    if let Some(m) = langs.iter().find_map(|l| l.month_number(word, None)) {
        return Some(u32::from(m));
    }
    if word.chars().count() < 3 {
        return None;
    }
    let mut found: Option<u32> = None;
    for l in langs {
        for (i, name) in l.month_names().iter().enumerate() {
            if name.starts_with(word) {
                let m = i as u32 + 1;
                if found.is_some_and(|f| f != m) {
                    return None;
                }
                found = Some(m);
            }
        }
    }
    found
}

fn num(t: &Token) -> Option<&str> {
    match &t.tok {
        Tok::Num(n) => Some(n),
        _ => None,
    }
}

fn is_sym(t: &Token, c: char) -> bool {
    t.tok == Tok::Sym(c)
}

fn small(n: &str) -> Option<u32> {
    if n.len() <= 2 {
        n.parse().ok()
    } else {
        None
    }
}

fn year4(n: &str) -> Option<i32> {
    if n.len() == 4 {
        n.parse().ok()
    } else {
        None
    }
}

/// Find the date mentions in a line.
fn find_mentions(line: &str, toks: &[Token], langs: &[&dyn ReportLanguage]) -> Vec<Mention> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if let Some((m, next)) = mention_at(line, toks, i, langs) {
            out.push(m);
            i = next;
        } else {
            i += 1;
        }
    }
    out
}

fn mention_at(
    line: &str,
    toks: &[Token],
    i: usize,
    langs: &[&dyn ReportLanguage],
) -> Option<(Mention, usize)> {
    let adjacent = |a: usize, b: usize| toks[a].end == toks[b].start;
    let mk = |start: usize, end: usize, day, month, year| Mention {
        start,
        end,
        day,
        month,
        year,
    };
    // Optional article in front of the date.
    let (first, art_start) = match &toks[i].tok {
        Tok::Word(w) if DATE_ARTICLES.contains(&w.as_str()) && i + 1 < toks.len() => {
            (i + 1, toks[i].start)
        }
        _ => (i, toks[i].start),
    };
    let t = |k: usize| toks.get(k);
    let first_start = if first == i { toks[i].start } else { art_start };

    if let Some(n0) = num(&toks[first]) {
        // 2026-11-24
        if let (Some(y), Some(a), Some(n1), Some(b), Some(n2)) = (
            year4(n0),
            t(first + 1),
            t(first + 2),
            t(first + 3),
            t(first + 4),
        ) {
            if is_sym(a, '-') && is_sym(b, '-') {
                if let (Some(mo), Some(d)) = (num(n1).and_then(small), num(n2).and_then(small)) {
                    if num(n1).is_some_and(|s| s.len() == 2)
                        && num(n2).is_some_and(|s| s.len() == 2)
                        && adjacent(first, first + 1)
                        && adjacent(first + 1, first + 2)
                        && adjacent(first + 2, first + 3)
                        && adjacent(first + 3, first + 4)
                    {
                        return Some((
                            mk(first_start, n2_end(toks, first + 4), d, mo, Some(y)),
                            first + 5,
                        ));
                    }
                }
            }
        }
        let day = small(n0)?;
        // 24.11.2026 or the range start 1.1.
        if let (Some(a), Some(n1), Some(b)) = (t(first + 1), t(first + 2), t(first + 3)) {
            if is_sym(a, '.') && is_sym(b, '.') && adjacent(first, first + 1) {
                if let Some(mo) = num(n1).and_then(small) {
                    if adjacent(first + 1, first + 2) && adjacent(first + 2, first + 3) {
                        let year = t(first + 4)
                            .filter(|_| adjacent(first + 3, first + 4))
                            .and_then(|y| num(y).and_then(year4));
                        if let Some(y) = year {
                            return Some((
                                mk(first_start, toks[first + 4].end, day, mo, Some(y)),
                                first + 5,
                            ));
                        }
                        if (1..=31).contains(&day) && (1..=12).contains(&mo) {
                            return Some((
                                mk(first_start, toks[first + 3].end, day, mo, None),
                                first + 4,
                            ));
                        }
                    }
                }
            }
        }
        // 24 November 2026 / 24. nov. 2026 / 24th of November
        let mut k = first + 1;
        let ordinal = |x: &Token| {
            is_sym(x, '.')
                || matches!(&x.tok, Tok::Word(w) if matches!(w.as_str(), "st" | "nd" | "rd" | "th"))
        };
        if t(k).is_some_and(|x| ordinal(x) && adjacent(k - 1, k)) {
            k += 1;
        }
        if t(k).is_some_and(|x| matches!(&x.tok, Tok::Word(w) if w == "of")) {
            k += 1;
        }
        let Some(Tok::Word(w)) = t(k).map(|x| x.tok.clone()) else {
            return None;
        };
        let month = month_of_word(langs, &w)?;
        let mut end_tok = k;
        if t(k + 1).is_some_and(|x| is_sym(x, '.')) && adjacent(k, k + 1) {
            end_tok = k + 1;
        }
        let mut y = end_tok + 1;
        if t(y).is_some_and(|x| is_sym(x, ',')) {
            y += 1;
        }
        if let Some(year) = t(y).and_then(num).and_then(year4) {
            return Some((mk(first_start, toks[y].end, day, month, Some(year)), y + 1));
        }
        return Some((
            mk(first_start, toks[end_tok].end, day, month, None),
            end_tok + 1,
        ));
    }

    // November 24, 2026
    if let Tok::Word(w) = &toks[first].tok {
        let month = month_of_word(langs, w)?;
        let mut k = first + 1;
        if t(k).is_some_and(|x| is_sym(x, '.')) && adjacent(k - 1, k) {
            k += 1;
        }
        let day = t(k).and_then(num).and_then(small)?;
        k += 1;
        if t(k).is_some_and(
            |x| matches!(&x.tok, Tok::Word(w) if matches!(w.as_str(), "st" | "nd" | "rd" | "th")),
        ) {
            k += 1;
        }
        if t(k).is_some_and(|x| is_sym(x, ',')) {
            k += 1;
        }
        let year = t(k).and_then(num).and_then(year4)?;
        return Some((mk(first_start, toks[k].end, day, month, Some(year)), k + 1));
    }
    let _ = line;
    None
}

fn n2_end(toks: &[Token], k: usize) -> usize {
    toks[k].end
}

/// A "date – date" range found in a line.
#[derive(Debug, Clone, Copy)]
struct Range {
    start: NaiveDate,
    end: NaiveDate,
}

/// What the dates of one line are.
struct LineDates {
    /// The report date: the first valid date that is not part of a range.
    report: Mention,
    /// Ranges, as valid dates.
    ranges: Vec<Range>,
    /// Byte spans of every mention (report date, range ends, others).
    spans: Vec<(usize, usize)>,
}

enum DateScan {
    NoDate,
    Invalid(String),
    Found(LineDates),
}

fn joins_range(between: &str) -> bool {
    let b = between.trim();
    matches!(b, "-" | "–" | "—" | "to" | "til" | "till")
}

fn scan_dates(line: &str, mentions: Vec<Mention>) -> DateScan {
    let spans: Vec<(usize, usize)> = mentions.iter().map(|m| (m.start, m.end)).collect();
    let mut ranges = Vec::new();
    let mut singles: Vec<Mention> = Vec::new();
    let mut invalid: Option<String> = None;
    let mut i = 0;
    while i < mentions.len() {
        let a = &mentions[i];
        if let Some(b) = mentions.get(i + 1) {
            if joins_range(&line[a.end..b.start]) && b.year.is_some() {
                let mut a2 = a.clone();
                if a2.year.is_none() {
                    // Year of the start: the end's, or the one before when the
                    // range wraps over New Year.
                    let by = b.year.unwrap_or_default();
                    a2.year = Some(if a2.month > b.month { by - 1 } else { by });
                }
                match (a2.date(), b.date()) {
                    (Some(start), Some(end)) => ranges.push(Range { start, end }),
                    _ => invalid = Some(line[a.start..b.end].to_string()),
                }
                i += 2;
                continue;
            }
        }
        if a.year.is_some() {
            singles.push(a.clone());
        }
        i += 1;
    }
    if let Some(m) = invalid {
        return DateScan::Invalid(m);
    }
    match singles.first() {
        None => DateScan::NoDate,
        Some(m) if m.date().is_none() => DateScan::Invalid(line[m.start..m.end].to_string()),
        Some(m) => DateScan::Found(LineDates {
            report: m.clone(),
            ranges,
            spans,
        }),
    }
}

/// An explicit `HH:MM` / `HH.MM` time in text where dates are blanked out;
/// returns the time and the byte span to remove (including `kl.`).
fn find_time(blanked: &str) -> Option<(NaiveTime, (usize, usize))> {
    let toks = tokenize(blanked);
    for i in 0..toks.len().saturating_sub(2) {
        let (Some(h), true, Some(m)) = (
            num(&toks[i]),
            matches!(toks[i + 1].tok, Tok::Sym(':' | '.')),
            num(&toks[i + 2]),
        ) else {
            continue;
        };
        if h.len() > 2
            || m.len() != 2
            || toks[i].end != toks[i + 1].start
            || toks[i + 1].end != toks[i + 2].start
        {
            continue;
        }
        // `12.30.2026`-like tails or `1.25.3` are not times.
        let glued = |c: Option<char>| c.is_some_and(|c| c.is_ascii_digit());
        if glued(blanked[toks[i + 2].end..].chars().next())
            || (matches!(blanked[toks[i + 2].end..].chars().next(), Some('.' | ':'))
                && glued(blanked[toks[i + 2].end + 1..].chars().next()))
        {
            continue;
        }
        let (Ok(h), Ok(m)) = (h.parse::<u32>(), m.parse::<u32>()) else {
            continue;
        };
        let Some(time) = NaiveTime::from_hms_opt(h, m, 0) else {
            continue;
        };
        let mut start = toks[i].start;
        // `kl. 08:00`
        let mut j = i;
        if j > 0 && is_sym(&toks[j - 1], '.') && j > 1 {
            if let Tok::Word(w) = &toks[j - 2].tok {
                if TIME_PREFIXES.contains(&w.as_str()) {
                    start = toks[j - 2].start;
                    j -= 2;
                }
            }
        } else if j > 0 {
            if let Tok::Word(w) = &toks[j - 1].tok {
                if TIME_PREFIXES.contains(&w.as_str()) {
                    start = toks[j - 1].start;
                }
            }
        }
        let _ = j;
        return Some((time, (start, toks[i + 2].end)));
    }
    None
}

/// Replace byte `spans` in `s` with spaces (same length, so offsets hold).
fn blank(s: &str, spans: &[(usize, usize)]) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, c) in s.char_indices() {
        if spans.iter().any(|&(a, b)| i >= a && i < b) {
            for _ in 0..c.len_utf8() {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn tidy_label(s: &str) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    joined
        .trim_matches(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '*' | '-' | '•' | '|' | ':' | ';' | ',' | '.' | '–' | '—' | '·'
                )
        })
        .to_string()
}

fn parse_line(line: &str, langs: &[&dyn ReportLanguage], item: &NewsItem) -> Option<CalendarEntry> {
    let toks = tokenize(line);
    let mentions = find_mentions(line, &toks, langs);
    if mentions.is_empty() {
        return None;
    }
    let dates = match scan_dates(line, mentions) {
        DateScan::NoDate => return None,
        DateScan::Invalid(what) => {
            warn!(news_id = %item.news_id, date = %what, line = %line.trim(), "invalid date in calendar release, line skipped");
            return None;
        }
        DateScan::Found(d) => d,
    };
    let report_date = dates.report.date()?;

    let all_blank = blank(line, &dates.spans);
    let (time, time_span) = match find_time(&all_blank) {
        Some((t, span)) => (Some(t), Some(span)),
        None => (None, None),
    };
    // The label keeps date ranges but not the report date, article or time.
    let mut remove = vec![(dates.report.start, dates.report.end)];
    remove.extend(time_span);
    let label = tidy_label(&blank(line, &remove));
    // Text for period detection: no dates (ranges are handled separately) and no time.
    let mut all = dates.spans.clone();
    all.extend(time_span);
    let text = blank(line, &all).to_lowercase();

    let kind = match classify_label(&label) {
        LabelClass::Report(kind, _) => kind,
        LabelClass::Ignored => return None,
        LabelClass::Unknown => {
            // Prose such as "the Q1 report is moved to 23 May 2025".
            if REPORT_WORDS.iter().any(|w| text.contains(w))
                && quarter_of(langs, &[], &text).is_some()
            {
                ReportKind::Interim
            } else {
                debug!(news_id = %item.news_id, line = %line.trim(), "calendar line is not a report");
                return None;
            }
        }
    };

    let fiscal_period = fiscal_period(kind, &text, &dates.ranges, langs)
        .filter(|p| plausible(p, report_date, item, &label));
    Some(CalendarEntry {
        date: report_date,
        time,
        label,
        kind,
        fiscal_period: fiscal_period.map(|p| p.render()),
    })
}

#[derive(Debug, Clone, Copy)]
struct Period {
    year: i32,
    quarter: Option<u32>,
}

impl Period {
    fn render(self) -> String {
        match self.quarter {
            Some(q) => format!("{}-Q{q}", self.year),
            None => self.year.to_string(),
        }
    }

    fn end(self) -> Option<NaiveDate> {
        match self.quarter {
            Some(q) => NaiveDate::from_ymd_opt(self.year, q * 3, 1)
                .and_then(|d| d.checked_add_months(chrono::Months::new(1)))
                .and_then(|d| d.pred_opt()),
            None => NaiveDate::from_ymd_opt(self.year, 12, 31),
        }
    }
}

/// A report is published after its period ends, and within a year of it.
/// Anything else is a typo in the release, and no period is better than a wrong one.
fn plausible(p: &Period, date: NaiveDate, item: &NewsItem, label: &str) -> bool {
    let ok = p
        .end()
        .is_some_and(|end| date > end && (date - end).num_days() <= 366);
    if !ok {
        warn!(news_id = %item.news_id, period = %p.render(), %date, label, "fiscal period does not fit the report date, left empty");
    }
    ok
}

/// The period a label names. A year-to-date range counts only when it starts on
/// 1 January: other starts are non-calendar fiscal years and are not mapped.
fn fiscal_period(
    kind: ReportKind,
    text: &str,
    ranges: &[Range],
    langs: &[&dyn ReportLanguage],
) -> Option<Period> {
    let ytd = ranges
        .iter()
        .find(|r| r.start.month() == 1 && r.start.day() == 1 && r.start.year() == r.end.year());
    let mut ys = years(text);
    ys.sort_unstable();
    ys.dedup();
    let text_year = match ys.as_slice() {
        [y] => y.parse::<i32>().ok(),
        _ => None,
    };
    match kind {
        ReportKind::Annual => {
            if let Some(r) = ytd.filter(|r| r.end.month() == 12) {
                return Some(Period {
                    year: r.end.year(),
                    quarter: None,
                });
            }
            text_year.map(|year| Period {
                year,
                quarter: None,
            })
        }
        ReportKind::Interim => {
            if let Some(q) = quarter_of(langs, &[], text) {
                let year = text_year.or(ytd.map(|r| r.end.year()))?;
                return Some(Period {
                    year,
                    quarter: Some(u32::from(q)),
                });
            }
            let r = ytd?;
            match r.end.month() {
                m @ (3 | 6 | 9) => Some(Period {
                    year: r.end.year(),
                    quarter: Some(m / 3),
                }),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(lang: &str, title: &str, text: &str) -> NewsItem {
        NewsItem {
            news_id: Uuid::nil(),
            url: None,
            lang: Some(lang.to_string()),
            tags: vec![CALENDAR_TAG.to_string()],
            title: title.to_string(),
            published_at: "2025-12-23T09:38:59Z".parse().unwrap(),
            author_slug: "x".into(),
            author_slugs: vec![],
            author_tickers: vec![],
            author_isins: vec![],
            source: None,
            attachment_urls: vec![],
            text: Some(text.to_string()),
        }
    }

    fn entries(text: &str) -> Vec<CalendarEntry> {
        parse_calendar_release(&item("en", "Financial calendar", text))
            .unwrap()
            .entries
    }

    fn one(text: &str) -> CalendarEntry {
        let e = entries(text);
        assert_eq!(e.len(), 1, "{text}: {e:?}");
        e.into_iter().next().unwrap()
    }

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn date_formats() {
        for text in [
            "24 November 2026 Interim report Q3 2026",
            "November 24, 2026 Interim report Q3 2026",
            "24 Nov 2026 Interim report Q3 2026",
            "24. november 2026 Delårsrapport Q3 2026",
            "24. nov. 2026 Delårsrapport Q3 2026",
            "den 24 november 2026 Delårsrapport Q3 2026",
            "2026-11-24 Interim report Q3 2026",
            "24.11.2026 Osavuosikatsaus Q3 2026",
            "24. marraskuuta 2026 Osavuosikatsaus Q3 2026",
            "Interim report Q3 2026: 24 November 2026",
        ] {
            let e = one(text);
            assert_eq!(e.date, d("2026-11-24"), "{text}");
            assert_eq!(e.kind, ReportKind::Interim, "{text}");
            assert_eq!(e.fiscal_period.as_deref(), Some("2026-Q3"), "{text}");
        }
    }

    #[test]
    fn layout_variants() {
        for text in [
            "* 24 November 2026 Interim report Q3 2026",
            "- 24 November 2026   Interim report Q3 2026",
            "  • 24 November 2026\tInterim report Q3 2026  ",
            "24 November 2026 | Interim report Q3 2026",
            "| 24 November 2026 | Interim report Q3 2026 |",
        ] {
            let e = one(text);
            assert_eq!(e.date, d("2026-11-24"), "{text}");
            assert_eq!(e.label, "Interim report Q3 2026", "{text}");
        }
    }

    #[test]
    fn invalid_date_skips_only_that_line() {
        let e =
            entries("31 June 2026 Interim report Q2 2026\n26 August 2026 Interim report Q2 2026");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].date, d("2026-08-26"));
        assert!(entries("31.02.2026 Osavuosikatsaus Q1 2026").is_empty());
    }

    #[test]
    fn fiscal_periods() {
        for (text, want) in [
            ("27 May 2026 Quarterly update Q1 2026", Some("2026-Q1")),
            ("24 November 2026 Interim report 2026-Q3", Some("2026-Q3")),
            (
                "24. november 2026 Delårsrapport 3. kvartal 2026",
                Some("2026-Q3"),
            ),
            (
                "24 November 2026 Interim report third quarter 2026",
                Some("2026-Q3"),
            ),
            (
                "24 November 2026 Quarterly update 1 January – 30 September 2026",
                Some("2026-Q3"),
            ),
            (
                "26 August 2026 Interim financial report 1 January - 30 June 2026",
                Some("2026-Q2"),
            ),
            (
                "27 May 2026 Quarterly update 1 January to 31 March 2026",
                Some("2026-Q1"),
            ),
            (
                "24. november 2026 Kvartalsopdatering 1. januar – 30. september 2026",
                Some("2026-Q3"),
            ),
            (
                "24.11.2026 Osavuosikatsaus 1.1. - 30.9.2026",
                Some("2026-Q3"),
            ),
            ("25 March 2026 Annual report 2025", Some("2025")),
            (
                "25 March 2026 Year-end report 1 January – 31 December 2025",
                Some("2025"),
            ),
            // non-calendar fiscal year: no guess
            ("2 September 2026 Interim report 1 May – 31 July 2026", None),
            (
                "2 September 2026 Interim report 1 April - 30 June 2026",
                None,
            ),
            // period that cannot fit the date: typo, no guess
            (
                "27 May 2026 Quarterly update 1 January – 31 March 2025",
                None,
            ),
        ] {
            let e = one(text);
            assert_eq!(e.fiscal_period.as_deref(), want, "{text}");
        }
        let e = one("2 September 2026 Interim report 1 May – 31 July 2026");
        assert!(e.label.contains("1 May – 31 July 2026"), "{}", e.label);
    }

    #[test]
    fn labels_in_every_language() {
        for (text, kind) in [
            ("1 March 2026 Quarterly update", ReportKind::Interim),
            ("1 March 2026 Trading statement", ReportKind::Interim),
            ("1 March 2026 Interim financial report", ReportKind::Interim),
            ("1. marts 2026 Kvartalsopdatering", ReportKind::Interim),
            ("1. marts 2026 Delårsrapport", ReportKind::Interim),
            ("1. marts 2026 Halvårsrapport", ReportKind::Interim),
            ("1. marts 2026 Årsrapport 2025", ReportKind::Annual),
            ("1 mars 2026 Kvartalsuppdatering", ReportKind::Interim),
            ("1.3.2026 Osavuosikatsaus", ReportKind::Interim),
            ("1.3.2026 Puolivuosikatsaus", ReportKind::Interim),
            ("1.3.2026 Tilinpäätöstiedote 2025", ReportKind::Annual),
            ("1.3.2026 Vuosikertomus 2025", ReportKind::Annual),
        ] {
            assert_eq!(one(text).kind, kind, "{text}");
        }
    }

    #[test]
    fn non_reports_give_no_entry() {
        for text in [
            "28 April 2026 Annual general meeting",
            "28. april 2026 Generalforsamling",
            "28.4.2026 Yhtiökokous",
            "29 April 2026 Ex-dividend date",
            "29 April 2026 Capital markets day",
            "Proposals from the shareholders for the annual general meeting 28 April 2026",
            "1 March 2026 Webcast",
        ] {
            assert!(entries(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn time_only_when_explicit() {
        assert_eq!(one("24 November 2026 Interim report Q3 2026").time, None);
        let t = |h, m| NaiveTime::from_hms_opt(h, m, 0);
        assert_eq!(
            one("24 November 2026 08:30 Interim report Q3 2026").time,
            t(8, 30)
        );
        assert_eq!(
            one("24.11.2026 klo 09.00 Osavuosikatsaus Q3 2026").time,
            t(9, 0)
        );
        let e = one("24. november 2026 kl. 08:30 Delårsrapport Q3 2026");
        assert_eq!(e.time, t(8, 30));
        assert_eq!(e.label, "Delårsrapport Q3 2026");
    }

    #[test]
    fn prose_numbers_are_ignored() {
        let text = "Board has adopted the financial calendar\nfor 20265:\n\n* 25 March 2026 Annual report 2025\n";
        assert_eq!(entries(text).len(), 1);
    }

    #[test]
    fn prose_only_change() {
        let e = one("The Q1 report is moved to 23 May 2025.");
        assert_eq!(e.date, d("2025-05-23"));
        assert_eq!(e.kind, ReportKind::Interim);
        // No year is written, and the release date is not a source for one.
        assert_eq!(e.fiscal_period, None);
        assert!(entries("The meeting is moved to 23 May 2025.").is_empty());
    }

    #[test]
    fn not_a_calendar_release_is_none() {
        let mut i = item("en", "Interim report Q3", "24 November 2026 Interim report");
        i.tags = vec![];
        assert!(parse_calendar_release(&i).is_none());
        // headline alone qualifies
        i.title = "Change in financial calendar for 2026".into();
        assert!(parse_calendar_release(&i).is_some());
    }

    #[test]
    fn empty_or_unreadable_release_is_some_and_empty() {
        let mut i = item("en", "Financial calendar 2026", "");
        i.text = None;
        assert!(parse_calendar_release(&i).unwrap().entries.is_empty());
        i.text = Some("See the attachment.".into());
        assert!(parse_calendar_release(&i).unwrap().entries.is_empty());
    }

    fn fixture(id_prefix: &str) -> NewsItem {
        let body = include_str!("../../tests/fixtures/mfn/news_firstfarms_calendar.json");
        let doc = super::super::feed::parse_document(body).unwrap();
        assert!(doc.bad_items.is_empty());
        doc.items
            .into_iter()
            .find(|i| i.news_id.to_string().starts_with(id_prefix))
            .unwrap()
    }

    fn summary(item: &NewsItem) -> Vec<(String, ReportKind, Option<String>)> {
        parse_calendar_release(item)
            .unwrap()
            .entries
            .iter()
            .map(|e| (e.date.to_string(), e.kind, e.fiscal_period.clone()))
            .collect()
    }

    #[test]
    fn firstfarms_2026_english() {
        use ReportKind::{Annual, Interim};
        let it = fixture("fa0cc072");
        let rc = parse_calendar_release(&it).unwrap();
        assert_eq!(rc.lang.as_deref(), Some("en"));
        let got = summary(&it);
        let s = |d: &str, k, p: &str| (d.to_string(), k, Some(p.to_string()));
        assert_eq!(
            got,
            vec![
                s("2026-03-25", Annual, "2025"),
                s("2026-05-27", Interim, "2026-Q1"),
                s("2026-08-26", Interim, "2026-Q2"),
                s("2026-11-24", Interim, "2026-Q3"),
            ]
        );
        assert_eq!(
            rc.entries[1].label,
            "Quarterly update 1 January – 31 March 2026"
        );
    }

    #[test]
    fn firstfarms_2026_danish_twin() {
        use ReportKind::{Annual, Interim};
        // The Danish release has `2025` in the two quarterly-update ranges
        // (`1. januar – 31. marts 2025`), a typo that cannot fit a report in
        // 2026: those periods stay empty rather than wrong.
        let got = summary(&fixture("deb62621"));
        assert_eq!(
            got,
            vec![
                ("2026-03-25".into(), Annual, Some("2025".into())),
                ("2026-05-27".into(), Interim, None),
                ("2026-08-26".into(), Interim, Some("2026-Q2".into())),
                ("2026-11-24".into(), Interim, None),
            ]
        );
    }

    #[test]
    fn firstfarms_2025_change_release() {
        use ReportKind::Interim;
        let s = |d: &str, p: &str| (d.to_string(), Interim, Some(p.to_string()));
        let want = vec![
            s("2025-05-23", "2025-Q1"),
            s("2025-08-27", "2025-Q2"),
            s("2025-11-26", "2025-Q3"),
        ];
        assert_eq!(summary(&fixture("ffd028d8")), want);
        assert_eq!(summary(&fixture("697d740a")), want);
    }
}
