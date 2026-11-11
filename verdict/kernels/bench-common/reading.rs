//! Reading and deciding, shared by the three bench kernels through `#[path]`
//! so that they differ only in how they find the labels.
//!
//! The format is the example company's: NOK amounts in millions, English with
//! comma thousands and dot decimals, Norwegian with space thousands and comma
//! decimals, the prior period introduced by "compared to/with" or "mot".

#![allow(dead_code)]

use verdict_sdk::{Decimal, Decision, Lang, Metric, Report, Verdict};

/// Metric keys, in the order of the lookups' label ends.
pub const KEYS: [&str; 3] = ["group_ebit", "farming_ebit", "wild_catch_ebit"];

const FIGURE_WINDOW: usize = 120;
const PRIOR_WINDOW: usize = 200;
const MAX_DIGITS: usize = 28;
const GROUP: usize = 0;
const FARMING: usize = 1;

/// A figure read after a label.
#[derive(Clone, Copy)]
pub struct Reading<'a> {
    pub current: Decimal,
    pub prior: Option<Decimal>,
    pub unit: &'a str,
}

/// Where each metric's label ends.
pub type Ends = [Option<usize>; 3];

fn is_space(c: char) -> bool {
    c == ' '
        || c == '\n'
        || c == '\r'
        || c == '\t'
        || c == '\u{a0}'
        || c == '\u{202f}'
        || c == '\u{2009}'
}

fn digits(bytes: &[u8], at: usize) -> usize {
    let mut end = at;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    end
}

fn comma_decimal(lang: Lang) -> bool {
    matches!(lang, Lang::No | Lang::Sv | Lang::Fi | Lang::Da)
}

fn group_separator(text: &str, at: usize, lang: Lang) -> Option<usize> {
    let first = text.get(at..)?.chars().next()?;
    if comma_decimal(lang) {
        matches!(first, ' ' | '\u{a0}' | '\u{202f}' | '\u{2009}').then_some(first.len_utf8())
    } else {
        (first == ',').then_some(1)
    }
}

fn parse_number(text: &str, at: usize, lang: Lang) -> Option<(Decimal, usize)> {
    let bytes = text.as_bytes();
    let rest = text.get(at..)?;
    let (negative, mut cursor) = match rest.chars().next()? {
        '-' => (true, at + 1),
        '\u{2212}' => (true, at + '\u{2212}'.len_utf8()),
        _ => (false, at),
    };
    let first_end = digits(bytes, cursor);
    if first_end == cursor {
        return None;
    }
    let first_start = cursor;
    let mut groups = vec![(first_start, first_end)];
    cursor = first_end;
    if first_end - first_start <= 3 {
        while let Some(sep) = group_separator(text, cursor, lang) {
            let start = cursor + sep;
            if digits(bytes, start) != start + 3 {
                break;
            }
            groups.push((start, start + 3));
            cursor = start + 3;
        }
    }
    let mark = if comma_decimal(lang) { b',' } else { b'.' };
    let mut scale = 0u32;
    if bytes.get(cursor) == Some(&mark) && bytes.get(cursor + 1).is_some_and(u8::is_ascii_digit) {
        let end = digits(bytes, cursor + 1);
        scale = u32::try_from(end - (cursor + 1)).ok()?;
        groups.push((cursor + 1, end));
        cursor = end;
    }
    if groups.iter().map(|(a, b)| b - a).sum::<usize>() > MAX_DIGITS {
        return None;
    }
    let mut mantissa: i128 = 0;
    for (a, b) in groups {
        for digit in bytes.get(a..b)? {
            mantissa = mantissa * 10 + i128::from(digit - b'0');
        }
    }
    if negative {
        mantissa = -mantissa;
    }
    Some((
        Decimal::try_from_i128_with_scale(mantissa, scale).ok()?,
        cursor,
    ))
}

fn clip(text: &str, from: usize, max: usize) -> &str {
    let rest = text.get(from..).unwrap_or("");
    let mut end = rest.len().min(max);
    while !rest.is_char_boundary(end) {
        end -= 1;
    }
    rest.get(..end).unwrap_or("")
}

fn amount(text: &str, from: usize, lang: Lang) -> Option<(Decimal, usize, &str)> {
    let nok = from + clip(text, from, FIGURE_WINDOW).find("NOK")? + 3;
    let rest = text.get(nok..)?;
    let skipped = rest.len() - rest.trim_start_matches(is_space).len();
    let (value, end) = parse_number(text, nok + skipped, lang)?;
    let after = text.get(end..)?;
    let after_spaces = after.trim_start_matches(is_space);
    let unit_start = end + (after.len() - after_spaces.len());
    let unit_len = after_spaces
        .find(|c: char| !c.is_alphabetic())
        .unwrap_or(after_spaces.len());
    let unit = text.get(unit_start..unit_start + unit_len)?;
    unit.starts_with("million")
        .then_some((value, unit_start + unit_len, unit))
}

fn is_word(text: &str, start: usize, len: usize) -> bool {
    let before = text.get(..start).and_then(|t| t.chars().next_back());
    let after = text.get(start + len..).and_then(|t| t.chars().next());
    before.is_none_or(|c| !c.is_alphanumeric()) && after.is_none_or(|c| !c.is_alphanumeric())
}

fn word_at(text: &str, start: usize, word: &str) -> Option<usize> {
    let end = start + word.len();
    (text.get(start..end)? == word && is_word(text, start, word.len())).then_some(end)
}

fn marker_end(sentence: &str) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (at, _) in sentence.match_indices("compared") {
        let Some(after) = word_at(sentence, at, "compared") else {
            continue;
        };
        let rest = sentence.get(after..)?;
        let next = after + (rest.len() - rest.trim_start_matches(is_space).len());
        let end = word_at(sentence, next, "to").or_else(|| word_at(sentence, next, "with"));
        if let Some(end) = end {
            best = Some(best.map_or(end, |kept| kept.min(end)));
        }
    }
    for (at, _) in sentence.match_indices("mot") {
        if let Some(end) = word_at(sentence, at, "mot") {
            best = Some(best.map_or(end, |kept| kept.min(end)));
        }
    }
    best
}

fn prior_after(text: &str, from: usize, lang: Lang) -> Option<Decimal> {
    let sentence = clip(text, from, PRIOR_WINDOW);
    let end = sentence
        .match_indices('.')
        .map(|(at, _)| at)
        .find(|&at| {
            sentence
                .get(at + 1..)
                .and_then(|t| t.chars().next())
                .is_none_or(is_space)
        })
        .unwrap_or(sentence.len());
    let sentence = sentence.get(..end)?;
    let marker = marker_end(sentence)?;
    amount(text, from + marker, lang).map(|(value, _, _)| value)
}

/// Reads the figure after a label ending at `end`, and its prior figure.
pub fn read(text: &str, end: usize, lang: Lang) -> Option<Reading<'_>> {
    let (current, unit_end, unit) = amount(text, end, lang)?;
    Some(Reading {
        current,
        prior: prior_after(text, unit_end, lang),
        unit,
    })
}

/// Reads every metric whose label was found.
pub fn read_all<'a>(text: &'a str, ends: &Ends, lang: Lang) -> [Option<Reading<'a>>; 3] {
    ends.map(|end| end.and_then(|end| read(text, end, lang)))
}

/// True when every metric was found and read.
pub fn complete(readings: &[Option<Reading>; 3]) -> bool {
    readings.iter().all(Option::is_some)
}

fn decide(readings: &[Option<Reading>; 3]) -> (Decision, String) {
    let abstain = |reason: &str| (Decision::Abstain, reason.to_owned());
    let names = ["group EBIT", "farming EBIT"];
    let mut moves = [Decimal::ZERO; 2];
    for (slot, (reading, name)) in moves.iter_mut().zip(readings.iter().zip(names)) {
        let Some(reading) = reading else {
            return abstain(&format!("{name} not found"));
        };
        let Some(prior) = reading.prior else {
            return abstain(&format!("prior {name} not found"));
        };
        *slot = reading.current - prior;
    }
    if moves[GROUP] > Decimal::ZERO && moves[FARMING] > Decimal::ZERO {
        (Decision::Buy, "group and farming EBIT up".to_owned())
    } else if moves[GROUP] < Decimal::ZERO && moves[FARMING] < Decimal::ZERO {
        (Decision::NoBuy, "group and farming EBIT down".to_owned())
    } else {
        abstain("mixed result")
    }
}

/// Builds the verdict from the readings.
pub fn verdict(readings: &[Option<Reading>; 3]) -> Verdict {
    let (decision, reason) = decide(readings);
    let metrics = KEYS
        .iter()
        .zip(readings)
        .filter_map(|(key, reading)| {
            reading.map(|reading| Metric {
                key,
                current: Some(reading.current.to_string()),
                prior: reading.prior.map(|prior| prior.to_string()),
                expected: None,
                unit: Some(reading.unit.to_owned()),
            })
        })
        .collect();
    Verdict {
        decision,
        reason,
        metrics,
    }
}

/// The text of a release, for the kernels' `analyze`.
pub fn analyze_with(report: &Report, find: impl Fn(&str, Lang) -> Ends) -> Verdict {
    let readings = read_all(report.text, &find(report.text, report.lang), report.lang);
    verdict(&readings)
}
