//! Five ways to find where a figure starts in this example company's report.
//!
//! Each function holds its **own** labels, lines or patterns, written for this
//! one company and its English and Swedish releases. There is no shared list:
//! what to look for (a heading, a bullet prefix, a phrase, a value) and how to
//! look for it is the kernel author's decision, taken from the company's
//! previous reports. Each returns the byte offset just past the label of net
//! sales and of EBIT, in that order.

use std::sync::LazyLock;

use aho_corasick::{AhoCorasick, AhoCorasickKind, MatchKind};
use memchr::{memchr, memmem};
use regex::Regex;
use verdict_sdk::Lang;

/// Where the label of net sales and of EBIT ends.
pub type Ends = [Option<usize>; 2];

fn word_start(text: &str, at: usize) -> bool {
    text.get(..at)
        .and_then(|before| before.chars().next_back())
        .is_none_or(|c| !c.is_alphanumeric())
}

fn word_end(text: &str, at: usize) -> bool {
    text.get(at..)
        .and_then(|after| after.chars().next())
        .is_none_or(|c| !c.is_alphanumeric())
}

fn next_line(text: &str, at: usize) -> Option<(&str, usize)> {
    let rest = text.get(at..).filter(|rest| !rest.is_empty())?;
    let len = memchr(b'\n', rest.as_bytes()).unwrap_or(rest.len());
    let line = rest.get(..len)?.trim_end_matches('\r');
    Some((line, at + len + 1))
}

/// (a) Structural position: the third line is the quarter's heading, and the
/// two lines after it are the net sales and EBIT bullets. Checks the heading
/// and each prefix with `starts_with`; `None` when any check fails, so the
/// caller can fall back to a broader lookup. Touches a few hundred bytes.
pub fn anchored(text: &str, lang: Lang) -> Option<Ends> {
    let (heading, bullets): (&str, [&str; 2]) = if lang == Lang::Sv {
        (
            "Tredje kvartalet",
            ["• Nettoomsättningen", "• Rörelseresultatet"],
        )
    } else if lang == Lang::En {
        ("Third quarter", ["• Net sales", "• Operating profit"])
    } else {
        return None;
    };
    let (_, at) = next_line(text, 0)?;
    let (_, at) = next_line(text, at)?;
    let (line, mut at) = next_line(text, at)?;
    if !line.starts_with(heading) {
        return None;
    }
    let mut ends: Ends = [None; 2];
    for (slot, prefix) in ends.iter_mut().zip(bullets) {
        let (line, next) = next_line(text, at)?;
        line.strip_prefix(prefix)?;
        *slot = Some(at + prefix.len());
        at = next;
    }
    Some(ends)
}

const WINDOW_BYTES: usize = 2048;

static FINDERS_EN: LazyLock<[memmem::Finder<'static>; 2]> = LazyLock::new(|| {
    [
        memmem::Finder::new("Net sales"),
        memmem::Finder::new("Operating profit"),
    ]
});
static FINDERS_SV: LazyLock<[memmem::Finder<'static>; 2]> = LazyLock::new(|| {
    [
        memmem::Finder::new("Nettoomsättning"),
        memmem::Finder::new("Rörelseresultat"),
    ]
});

/// (b) A single anchor in a known window: one `memmem` per label over the first
/// 2 KB, with the exact casing this company uses.
pub fn window(text: &str, lang: Lang) -> Ends {
    let finders = if lang == Lang::Sv {
        &*FINDERS_SV
    } else {
        &*FINDERS_EN
    };
    let hay = text
        .as_bytes()
        .get(..WINDOW_BYTES)
        .unwrap_or(text.as_bytes());
    let mut ends: Ends = [None; 2];
    for (slot, finder) in ends.iter_mut().zip(finders) {
        *slot = finder
            .find_iter(hay)
            .find(|start| word_start(text, *start))
            .map(|start| start + finder.needle().len());
    }
    ends
}

/// An Aho-Corasick pattern: the phrase, its metric, and whether it is an open
/// stem (`revenue` also matches `revenues`).
const AUTOMATON_PATTERNS: [(&str, usize, bool); 8] = [
    ("net sales", 0, false),
    ("revenue", 0, true),
    ("nettoomsättning", 0, true),
    ("operating profit", 1, false),
    ("ebit", 1, false),
    ("rörelseresultat", 1, true),
    ("liikevaihto", 0, true),
    ("liikevoitto", 1, true),
];

static AUTOMATON: LazyLock<Option<AhoCorasick>> = LazyLock::new(|| {
    AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .match_kind(MatchKind::LeftmostLongest)
        .kind(Some(AhoCorasickKind::DFA))
        .build(AUTOMATON_PATTERNS.map(|(phrase, _, _)| phrase))
        .ok()
});

/// (c) Several labels: one Aho-Corasick pass, built once, stopping as soon as
/// both are found. The fallback when (a) fails.
pub fn automaton(text: &str) -> Ends {
    let mut ends: Ends = [None; 2];
    let Some(automaton) = &*AUTOMATON else {
        return ends;
    };
    for found in automaton.find_iter(text) {
        let Some(&(_, metric, open)) = AUTOMATON_PATTERNS.get(found.pattern().as_usize()) else {
            continue;
        };
        let Some(slot) = ends.get_mut(metric) else {
            continue;
        };
        if slot.is_none()
            && word_start(text, found.start())
            && (open || word_end(text, found.end()))
        {
            *slot = Some(found.end());
        }
        if ends.iter().all(Option::is_some) {
            break;
        }
    }
    ends
}

static REGEXES: LazyLock<[Option<Regex>; 2]> = LazyLock::new(|| {
    [
        Regex::new(r"(?i)\b(?:net sales|revenue\w*|nettoomsättning\w*)").ok(),
        Regex::new(r"(?i)\b(?:operating profit|ebit\b|rörelseresultat\w*)").ok(),
    ]
});

/// (d) Regex: one per metric, compiled once. Only for when nothing above works.
pub fn pattern(text: &str) -> Ends {
    let mut ends: Ends = [None; 2];
    for (slot, regex) in ends.iter_mut().zip(REGEXES.iter()) {
        *slot = regex
            .as_ref()
            .and_then(|regex| regex.find(text))
            .map(|found| found.end());
    }
    ends
}

/// (e) `to_lowercase` + `find`: the slow way, kept to measure against. It copies
/// the text, and when lowercasing changes the text's length every offset would
/// be wrong, so it then finds nothing.
pub fn lowercase(text: &str) -> Ends {
    const LABELS: [&[(&str, bool)]; 2] = [
        &[
            ("net sales", false),
            ("revenue", true),
            ("nettoomsättning", true),
        ],
        &[
            ("operating profit", false),
            ("ebit", false),
            ("rörelseresultat", true),
        ],
    ];
    let lower = text.to_lowercase();
    let mut ends: Ends = [None; 2];
    if lower.len() != text.len() {
        return ends;
    }
    for (slot, labels) in ends.iter_mut().zip(LABELS) {
        *slot = labels
            .iter()
            .filter_map(|(phrase, open)| {
                lower
                    .match_indices(phrase)
                    .map(|(start, _)| (start, start + phrase.len()))
                    .find(|&(start, end)| {
                        word_start(&lower, start) && (*open || word_end(&lower, end))
                    })
            })
            .min()
            .map(|(_, end)| end);
    }
    ends
}
