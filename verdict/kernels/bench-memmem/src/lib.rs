//! Benchmark kernel, rung 2 of the lookup ladder: one `memmem::Finder` per
//! label, built once, over a known window, with the exact casing the example
//! company uses. Reading and deciding are shared with the other bench kernels.

use std::sync::LazyLock;

use memchr::memmem::Finder;
use verdict_sdk::{export_kernel, Lang, Report, Verdict};

#[path = "../../bench-common/reading.rs"]
mod reading;

use reading::Ends;

const WINDOW_BYTES: usize = 6 * 1024;

const LABELS_EN: [&[&str]; 3] = [
    &["Operational EBIT"],
    &[
        "Operational EBIT in Farming",
        "Operational EBIT in the farming segment",
    ],
    &[
        "Operational EBIT in Wild Catch",
        "The wild catch segment delivered an operational EBIT of",
    ],
];

const LABELS_NO: [&[&str]; 3] = [
    &["Operasjonelt driftsresultat"],
    &[
        "Operasjonelt driftsresultat i Havbruk",
        "Operasjonelt driftsresultat i havbrukssegmentet",
    ],
    &[
        "Operasjonelt driftsresultat i Villfangst",
        "Villfangstsegmentet leverte et operasjonelt driftsresultat på",
    ],
];

type Finders = Vec<Vec<Finder<'static>>>;

fn build(labels: &[&[&str]; 3]) -> Finders {
    labels
        .iter()
        .map(|phrases| {
            phrases
                .iter()
                .map(|phrase| Finder::new(phrase).into_owned())
                .collect()
        })
        .collect()
}

static FINDERS_EN: LazyLock<Finders> = LazyLock::new(|| build(&LABELS_EN));
static FINDERS_NO: LazyLock<Finders> = LazyLock::new(|| build(&LABELS_NO));

fn find(text: &str, lang: Lang) -> Ends {
    let finders = if lang == Lang::No {
        &*FINDERS_NO
    } else {
        &*FINDERS_EN
    };
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

/// Reads the headline EBIT figures with one `memmem` per label in the first
/// 6 KB, and applies the placeholder decision rule.
pub fn analyze(report: &Report) -> Verdict {
    reading::analyze_with(report, find)
}

export_kernel!(crate::analyze);
