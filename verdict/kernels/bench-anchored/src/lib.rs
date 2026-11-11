//! Benchmark kernel, rung 1 of the lookup ladder: the figures sit in the
//! first sentence of known paragraphs. Walks the paragraphs, checks each one's
//! prefix with `starts_with`, and reads the number right after it. When any
//! metric is not found and read there (the company changed its wording), it
//! falls back to one `memmem` window, and abstains if that fails too.
//! Reading and deciding are shared with the other bench kernels.

use std::sync::LazyLock;

use memchr::memmem::Finder;
use verdict_sdk::{export_kernel, Lang, Report, Verdict};

#[path = "../../bench-common/reading.rs"]
mod reading;

use reading::{Ends, Reading};

const MAX_PARAGRAPHS: usize = 60;
const WINDOW_BYTES: usize = 6 * 1024;

const GROUP_EN: &str = "Operational EBIT";
const GROUP_NO: &str = "Operasjonelt driftsresultat";

const PREFIXES_EN: [&[&str]; 2] = [
    &[
        "Operational EBIT in Farming",
        "Operational EBIT in the farming segment",
    ],
    &[
        "Operational EBIT in Wild Catch",
        "The wild catch segment delivered an operational EBIT of",
    ],
];

const PREFIXES_NO: [&[&str]; 2] = [
    &[
        "Operasjonelt driftsresultat i Havbruk",
        "Operasjonelt driftsresultat i havbrukssegmentet",
    ],
    &[
        "Operasjonelt driftsresultat i Villfangst",
        "Villfangstsegmentet leverte et operasjonelt driftsresultat på",
    ],
];

static BREAK: LazyLock<Finder<'static>> = LazyLock::new(|| Finder::new("\n\n").into_owned());

fn is_quote(c: char) -> bool {
    matches!(c, '"' | '“' | '«')
}

fn anchored(text: &str, lang: Lang) -> Ends {
    let (group, prefixes) = if lang == Lang::No {
        (GROUP_NO, &PREFIXES_NO)
    } else {
        (GROUP_EN, &PREFIXES_EN)
    };
    let mut ends: Ends = [None; 3];
    let bytes = text.as_bytes();
    let mut start = 0;
    for _ in 0..MAX_PARAGRAPHS {
        while bytes.get(start) == Some(&b'\n') {
            start += 1;
        }
        let Some(paragraph) = text.get(start..).filter(|rest| !rest.is_empty()) else {
            break;
        };
        if ends[0].is_none() {
            if let Some(rest) = paragraph.strip_prefix(is_quote) {
                if rest.starts_with(group) {
                    ends[0] = Some(start + paragraph.len() - rest.len() + group.len());
                }
            }
        }
        for (slot, phrases) in ends[1..].iter_mut().zip(prefixes) {
            if slot.is_none() {
                if let Some(phrase) = phrases
                    .iter()
                    .find(|phrase| paragraph.starts_with(**phrase))
                {
                    *slot = Some(start + phrase.len());
                }
            }
        }
        match BREAK.find(paragraph.as_bytes()) {
            Some(at) => start += at + 2,
            None => break,
        }
    }
    ends
}

fn windowed(text: &str, lang: Lang) -> Ends {
    static EN: LazyLock<Vec<Vec<Finder<'static>>>> =
        LazyLock::new(|| finders(GROUP_EN, &PREFIXES_EN));
    static NO: LazyLock<Vec<Vec<Finder<'static>>>> =
        LazyLock::new(|| finders(GROUP_NO, &PREFIXES_NO));
    let finders = if lang == Lang::No { &*NO } else { &*EN };
    let mut size = text.len().min(WINDOW_BYTES);
    while !text.is_char_boundary(size) {
        size -= 1;
    }
    let window = text.as_bytes().get(..size).unwrap_or(text.as_bytes());
    let mut ends: Ends = [None; 3];
    for (slot, metric) in ends.iter_mut().zip(finders.iter()) {
        *slot = metric
            .iter()
            .filter_map(|finder| finder.find(window).map(|at| at + finder.needle().len()))
            .min();
    }
    ends
}

fn finders(group: &'static str, prefixes: &[&[&'static str]; 2]) -> Vec<Vec<Finder<'static>>> {
    let mut all = vec![vec![Finder::new(group).into_owned()]];
    for phrases in prefixes {
        all.push(
            phrases
                .iter()
                .map(|phrase| Finder::new(phrase).into_owned())
                .collect(),
        );
    }
    all
}

fn readings<'a>(text: &'a str, lang: Lang) -> [Option<Reading<'a>>; 3] {
    let found = reading::read_all(text, &anchored(text, lang), lang);
    if reading::complete(&found) {
        return found;
    }
    reading::read_all(text, &windowed(text, lang), lang)
}

/// Reads the headline EBIT figures at their known paragraphs, falling back to
/// one `memmem` window, and applies the placeholder decision rule.
pub fn analyze(report: &Report) -> Verdict {
    reading::verdict(&readings(report.text, report.lang))
}

export_kernel!(crate::analyze);
