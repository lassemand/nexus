//! Benchmark kernel, rung 3 of the lookup ladder: one Aho-Corasick pass over
//! all labels of both languages, built once, stopping as soon as every metric
//! has a hit. Reading and deciding are shared with the other bench kernels.

use std::sync::LazyLock;

use aho_corasick::{AhoCorasick, AhoCorasickKind, MatchKind};
use verdict_sdk::{export_kernel, Lang, Report, Verdict};

#[path = "../../bench-common/reading.rs"]
mod reading;

use reading::Ends;

const PATTERNS: [(&str, usize); 10] = [
    ("Operational EBIT", 0),
    ("Operasjonelt driftsresultat", 0),
    ("Operational EBIT in Farming", 1),
    ("Operational EBIT in the farming segment", 1),
    ("Operasjonelt driftsresultat i Havbruk", 1),
    ("Operasjonelt driftsresultat i havbrukssegmentet", 1),
    ("Operational EBIT in Wild Catch", 2),
    ("The wild catch segment delivered an operational EBIT of", 2),
    ("Operasjonelt driftsresultat i Villfangst", 2),
    (
        "Villfangstsegmentet leverte et operasjonelt driftsresultat på",
        2,
    ),
];

static AUTOMATON: LazyLock<Option<AhoCorasick>> = LazyLock::new(|| {
    AhoCorasick::builder()
        .match_kind(MatchKind::LeftmostLongest)
        .kind(Some(AhoCorasickKind::DFA))
        .build(PATTERNS.map(|(phrase, _)| phrase))
        .ok()
});

fn find(text: &str, _lang: Lang) -> Ends {
    let mut ends: Ends = [None; 3];
    let Some(automaton) = &*AUTOMATON else {
        return ends;
    };
    for hit in automaton.find_iter(text) {
        let Some(&(_, metric)) = PATTERNS.get(hit.pattern().as_usize()) else {
            continue;
        };
        if let Some(slot) = ends.get_mut(metric) {
            if slot.is_none() {
                *slot = Some(hit.end());
            }
        }
        if ends.iter().all(Option::is_some) {
            break;
        }
    }
    ends
}

/// Reads the headline EBIT figures with one Aho-Corasick pass, and applies the
/// placeholder decision rule.
pub fn analyze(report: &Report) -> Verdict {
    reading::analyze_with(report, find)
}

export_kernel!(crate::analyze);
